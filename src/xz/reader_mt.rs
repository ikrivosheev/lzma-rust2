use std::{
    collections::BTreeMap,
    io::{self, Cursor, Seek, SeekFrom},
    sync::{
        atomic::{AtomicBool, AtomicU32, Ordering},
        mpsc::{self, Receiver, SyncSender},
        Arc, Mutex,
    },
    thread,
};

use super::{create_filter_chain, BlockHeader, CheckType, Index, StreamFooter, StreamHeader};
use crate::{
    error_invalid_data, set_error,
    work_queue::{WorkStealingQueue, WorkerHandle},
    ByteReader, Read,
};

#[derive(Debug, Clone)]
struct XZBlock {
    start_pos: u64,
    unpadded_size: u64,
    uncompressed_size: u64,
}

/// A work unit for a worker thread.
/// Contains the sequence number and block data.
type WorkUnit = (u64, Vec<u8>);

/// A result unit from a worker thread.
/// Contains the sequence number and the decompressed data.
type ResultUnit = (u64, Vec<u8>);

enum State {
    /// Dispatching blocks to worker threads.
    Dispatching,
    /// All blocks dispatched, waiting for workers to complete.
    Draining,
    /// All data has been decompressed and returned. The stream is exhausted.
    Finished,
    /// A fatal error occurred in either the reader or a worker thread.
    Error,
}

/// A multi-threaded XZ decompressor.
pub struct XZReaderMT<R: Read + Seek> {
    inner: Option<R>,
    blocks: Vec<XZBlock>,
    check_type: CheckType,
    result_rx: Receiver<ResultUnit>,
    result_tx: SyncSender<ResultUnit>,
    next_sequence_to_dispatch: u64,
    next_sequence_to_return: u64,
    last_sequence_id: Option<u64>,
    out_of_order_chunks: BTreeMap<u64, Vec<u8>>,
    current_chunk: Cursor<Vec<u8>>,
    shutdown_flag: Arc<AtomicBool>,
    error_store: Arc<Mutex<Option<io::Error>>>,
    state: State,
    work_queue: WorkStealingQueue<WorkUnit>,
    active_workers: Arc<AtomicU32>,
    max_workers: u32,
    worker_handles: Vec<thread::JoinHandle<()>>,
    allow_multiple_streams: bool,
}

impl<R: Read + Seek> XZReaderMT<R> {
    /// Creates a new multi-threaded XZ reader.
    ///
    /// - `inner`: The reader to read compressed data from. Must implement Seek.
    /// - `allow_multiple_streams`: Whether to allow reading multiple XZ streams concatenated together.
    /// - `num_workers`: The maximum number of worker threads for decompression. Currently capped at 256 Threads.
    pub fn new(inner: R, allow_multiple_streams: bool, num_workers: u32) -> io::Result<Self> {
        let max_workers = num_workers.clamp(1, 256);

        let work_queue = WorkStealingQueue::new();
        let (result_tx, result_rx) = mpsc::sync_channel::<ResultUnit>(1);
        let shutdown_flag = Arc::new(AtomicBool::new(false));
        let error_store = Arc::new(Mutex::new(None));
        let active_workers = Arc::new(AtomicU32::new(0));

        let mut reader = Self {
            inner: Some(inner),
            blocks: Vec::new(),
            check_type: CheckType::None,
            result_rx,
            result_tx,
            next_sequence_to_dispatch: 0,
            next_sequence_to_return: 0,
            last_sequence_id: None,
            out_of_order_chunks: BTreeMap::new(),
            current_chunk: Cursor::new(Vec::new()),
            shutdown_flag,
            error_store,
            state: State::Dispatching,
            work_queue,
            active_workers,
            max_workers,
            worker_handles: Vec::new(),
            allow_multiple_streams,
        };

        reader.scan_blocks()?;

        Ok(reader)
    }

    /// Scan the XZ file to collect information about all blocks.
    /// This reads the index at the end of the file to efficiently locate block boundaries.
    fn scan_blocks(&mut self) -> io::Result<()> {
        let mut reader = self.inner.take().expect("inner reader not set");

        let stream_header = StreamHeader::parse(&mut reader)?;
        self.check_type = stream_header.check_type;

        let header_end_pos = reader.stream_position()?;

        let file_size = reader.seek(SeekFrom::End(0))?;

        // Minimum XZ file: 12 byte header + 12 byte footer + 8 byte minimum index.
        if file_size < 32 {
            return Err(error_invalid_data(
                "File too small to contain a valid XZ stream",
            ));
        }

        reader.seek(SeekFrom::End(-12))?;

        let stream_footer = StreamFooter::parse(&mut reader)?;

        let header_flags = [0, self.check_type as u8];

        if stream_footer.stream_flags != header_flags {
            return Err(error_invalid_data(
                "stream header and footer flags mismatch",
            ));
        }

        // Now read the index using backward size.
        let index_size = (stream_footer.backward_size + 1) * 4;
        let index_start_pos = file_size - 12 - index_size as u64;

        reader.seek(SeekFrom::Start(index_start_pos))?;

        // Parse the index.
        let index_indicator = reader.read_u8()?;

        if index_indicator != 0 {
            return Err(error_invalid_data("invalid XZ index indicator"));
        }

        let index = Index::parse(&mut reader)?;

        let mut block_start_pos = header_end_pos;

        for record in &index.records {
            self.blocks.push(XZBlock {
                start_pos: block_start_pos,
                unpadded_size: record.unpadded_size,
                uncompressed_size: record.uncompressed_size,
            });

            let padding_needed = (4 - (record.unpadded_size % 4)) % 4;
            let actual_block_size = record.unpadded_size + padding_needed;

            block_start_pos += actual_block_size;
        }

        if self.blocks.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "No valid XZ blocks found",
            ));
        }

        self.inner = Some(reader);
        Ok(())
    }

    fn spawn_worker_thread(&mut self) {
        let worker_handle = self.work_queue.worker();
        let result_tx = self.result_tx.clone();
        let shutdown_flag = Arc::clone(&self.shutdown_flag);
        let error_store = Arc::clone(&self.error_store);
        let active_workers = Arc::clone(&self.active_workers);
        let check_type = self.check_type;

        let handle = thread::spawn(move || {
            worker_thread_logic(
                worker_handle,
                result_tx,
                check_type,
                shutdown_flag,
                error_store,
                active_workers,
            );
        });

        self.worker_handles.push(handle);
    }

    /// Get the count of XZ blocks found in the file.
    pub fn block_count(&self) -> usize {
        self.blocks.len()
    }

    fn dispatch_next_block(&mut self) -> io::Result<bool> {
        let block_index = self.next_sequence_to_dispatch as usize;

        if block_index >= self.blocks.len() {
            // No more blocks to dispatch.
            return Ok(false);
        }

        let block = &self.blocks[block_index];
        let mut reader = self.inner.take().expect("inner reader not set");

        reader.seek(SeekFrom::Start(block.start_pos))?;

        let padding_needed = (4 - (block.unpadded_size % 4)) % 4;
        let total_block_size = block.unpadded_size + padding_needed;

        let mut block_data = vec![0u8; total_block_size as usize];
        reader.read_exact(&mut block_data)?;

        self.inner = Some(reader);

        if !self
            .work_queue
            .push((self.next_sequence_to_dispatch, block_data))
        {
            // Queue is closed, this indicates shutdown.
            self.state = State::Error;
            set_error(
                io::Error::new(io::ErrorKind::BrokenPipe, "Worker threads have shut down"),
                &self.error_store,
                &self.shutdown_flag,
            );
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "Worker threads have shut down",
            ));
        }

        // We spawn a new thread if we have work queued, no available workers, and haven't reached
        // the maximal allowed parallelism yet.
        let spawned_workers = self.worker_handles.len() as u32;
        let active_workers = self.active_workers.load(Ordering::Acquire);
        let queue_len = self.work_queue.len();

        if queue_len > 0 && active_workers == spawned_workers && spawned_workers < self.max_workers
        {
            self.spawn_worker_thread();
        }

        self.next_sequence_to_dispatch += 1;
        Ok(true)
    }

    fn get_next_uncompressed_chunk(&mut self) -> io::Result<Option<Vec<u8>>> {
        loop {
            // Always check for already-received chunks first.
            if let Some(result) = self
                .out_of_order_chunks
                .remove(&self.next_sequence_to_return)
            {
                self.next_sequence_to_return += 1;
                return Ok(Some(result));
            }

            // Check for a globally stored error.
            if let Some(err) = self.error_store.lock().unwrap().take() {
                self.state = State::Error;
                return Err(err);
            }

            match self.state {
                State::Dispatching => {
                    // First, always try to receive a result without blocking.
                    // This keeps the pipeline moving and avoids unnecessary blocking.
                    match self.result_rx.try_recv() {
                        Ok((seq, result)) => {
                            if seq == self.next_sequence_to_return {
                                self.next_sequence_to_return += 1;
                                return Ok(Some(result));
                            } else {
                                self.out_of_order_chunks.insert(seq, result);
                                continue; // Loop again to check the out_of_order_chunks.
                            }
                        }
                        Err(mpsc::TryRecvError::Disconnected) => {
                            // All workers are done.
                            self.state = State::Draining;
                            continue;
                        }
                        Err(mpsc::TryRecvError::Empty) => {
                            // No results are ready. Now, we can consider dispatching more work.
                        }
                    }

                    // If the work queue has capacity, try to read more from the source.
                    if self.work_queue.is_empty() {
                        match self.dispatch_next_block() {
                            Ok(true) => {
                                // Successfully read and dispatched a block, loop to continue.
                                continue;
                            }
                            Ok(false) => {
                                // No more blocks to dispatch.
                                // Set the last sequence ID and transition to draining.
                                self.last_sequence_id =
                                    Some(self.next_sequence_to_dispatch.saturating_sub(1));
                                self.state = State::Draining;
                                continue;
                            }
                            Err(error) => {
                                set_error(error, &self.error_store, &self.shutdown_flag);
                                self.state = State::Error;
                                continue;
                            }
                        }
                    }

                    // Now we MUST wait for a result to make progress.
                    match self.result_rx.recv() {
                        Ok((seq, result)) => {
                            if seq == self.next_sequence_to_return {
                                self.next_sequence_to_return += 1;
                                return Ok(Some(result));
                            } else {
                                self.out_of_order_chunks.insert(seq, result);
                                // We've made progress, loop to check the out_of_order_chunks.
                                continue;
                            }
                        }
                        Err(_) => {
                            // All workers are done.
                            self.state = State::Draining;
                        }
                    }
                }
                State::Draining => {
                    if let Some(last_seq) = self.last_sequence_id {
                        if self.next_sequence_to_return > last_seq {
                            self.state = State::Finished;
                            continue;
                        }
                    }

                    // In Draining state, we only wait for results.
                    match self.result_rx.recv() {
                        Ok((seq, result)) => {
                            if seq == self.next_sequence_to_return {
                                self.next_sequence_to_return += 1;
                                return Ok(Some(result));
                            } else {
                                self.out_of_order_chunks.insert(seq, result);
                            }
                        }
                        Err(_) => {
                            // All workers finished, and channel is empty. We are done.
                            self.state = State::Finished;
                        }
                    }
                }
                State::Finished => {
                    return Ok(None);
                }
                State::Error => {
                    // The error was already logged, now we just propagate it.
                    return Err(self.error_store.lock().unwrap().take().unwrap_or_else(|| {
                        io::Error::other("decompression failed with an unknown error")
                    }));
                }
            }
        }
    }
}

/// The logic for a single worker thread.
fn worker_thread_logic(
    worker_handle: WorkerHandle<WorkUnit>,
    result_tx: SyncSender<ResultUnit>,
    check_type: CheckType,
    shutdown_flag: Arc<AtomicBool>,
    error_store: Arc<Mutex<Option<io::Error>>>,
    active_workers: Arc<AtomicU32>,
) {
    while !shutdown_flag.load(Ordering::Acquire) {
        let (seq, work_unit_data) = match worker_handle.steal() {
            Some(work) => {
                active_workers.fetch_add(1, Ordering::Release);
                work
            }
            None => {
                // No more work available and queue is closed
                break;
            }
        };

        let result = decompress_xz_block(work_unit_data, check_type);

        match result {
            Ok(decompressed_data) => {
                if result_tx.send((seq, decompressed_data)).is_err() {
                    active_workers.fetch_sub(1, Ordering::Release);
                    return;
                }
            }
            Err(error) => {
                active_workers.fetch_sub(1, Ordering::Release);
                set_error(error, &error_store, &shutdown_flag);
                return;
            }
        }

        active_workers.fetch_sub(1, Ordering::Release);
    }
}

/// Decompresses a single XZ block by parsing the header and applying filters directly.
fn decompress_xz_block(block_data: Vec<u8>, check_type: CheckType) -> io::Result<Vec<u8>> {
    let (filters, properties, header_size) = BlockHeader::parse_from_slice(&block_data)?;

    let checksum_size = check_type.checksum_size() as usize;
    let padding_in_block_data = (4 - (block_data.len() % 4)) % 4;
    let unpadded_size_in_data = block_data.len() - padding_in_block_data;
    let compressed_data_end = unpadded_size_in_data - checksum_size;

    if compressed_data_end <= header_size {
        return Err(error_invalid_data(
            "Block data too short for compressed content",
        ));
    }

    let compressed_data = block_data[header_size..compressed_data_end].to_vec();
    let mut compressed_data = compressed_data.as_slice();

    let base_reader: Box<dyn Read> = Box::new(&mut compressed_data);
    let mut chain_reader = create_filter_chain(base_reader, &filters, &properties);

    let mut decompressed_data = Vec::new();
    chain_reader.read_to_end(&mut decompressed_data)?;

    Ok(decompressed_data)
}

impl<R: Read + Seek> Read for XZReaderMT<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }

        let bytes_read = self.current_chunk.read(buf)?;

        if bytes_read > 0 {
            return Ok(bytes_read);
        }

        let chunk_data = self.get_next_uncompressed_chunk()?;

        let Some(chunk_data) = chunk_data else {
            // This is the clean end of the stream.
            return Ok(0);
        };

        self.current_chunk = Cursor::new(chunk_data);

        // Recursive call to read the new chunk data.
        self.read(buf)
    }
}

impl<R: Read + Seek> Drop for XZReaderMT<R> {
    fn drop(&mut self) {
        self.shutdown_flag.store(true, Ordering::Release);
        self.work_queue.close();
        // Worker threads will exit when the work queue is closed.
        // JoinHandles will be dropped, which is fine since we set the shutdown flag,
    }
}

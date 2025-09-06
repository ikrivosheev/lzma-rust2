use std::{
    env,
    fs::File,
    io::{self, BufReader},
    time::Instant,
};

use lzma_rust2::LzmaReader;

fn main() -> io::Result<()> {
    let mut args = env::args();

    let input = BufReader::new(File::open(args.nth(1).unwrap())?);
    let mut output = File::create(args.next().unwrap())?;
    let input_len = input.get_ref().metadata()?.len();
    let start = Instant::now();
    let mut reader = LzmaReader::new_mem_limit(input, u32::MAX, None)?;
    io::copy(&mut reader, &mut output)?;

    println!("{input_len} in");
    println!("{} out", output.metadata()?.len());
    println!("{:?}", start.elapsed());
    Ok(())
}

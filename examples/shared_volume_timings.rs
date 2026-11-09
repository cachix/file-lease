//! Overlapping independent leases, with one live anchor retaining the volume.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn main() -> std::io::Result<()> {
    use file_lease::FileLease;
    use std::{
        collections::HashSet,
        io,
        sync::{Arc, Barrier},
        time::Instant,
    };
    if let Some(code) = file_lease::maybe_run_keeper() {
        std::process::exit(code);
    }
    let mut samples = 10;
    let mut parallel = 4;
    let mut payload_mib = 0;
    let mut arguments = std::env::args().skip(1);
    while let Some(argument) = arguments.next() {
        let destination = match argument.as_str() {
            "--samples" => &mut samples,
            "--parallel" => &mut parallel,
            "--payload-mib" => &mut payload_mib,
            _ => return Err(io::Error::other("Unknown overlap benchmark option")),
        };
        *destination = arguments
            .next()
            .and_then(|value| value.parse::<usize>().ok())
            .filter(|value| (1..=1000).contains(value))
            .ok_or_else(|| io::Error::other("Benchmark counts must be between 1 and 1000"))?;
    }
    if payload_mib > 40 {
        return Err(io::Error::other("Payload must be at most 40 MiB"));
    }
    let payload_size = if payload_mib == 0 {
        256
    } else {
        payload_mib * 1024 * 1024
    };
    let make_lease = || {
        FileLease::new(std::process::id()).with_keeper_executable(std::env::current_exe().unwrap())
    };
    let mut volumes = HashSet::new();
    println!("mode,sample,operation,index,elapsed_ms,volumes");
    let start = Instant::now();
    let anchor = make_lease();
    let anchor_path = anchor.materialize(&vec![42; payload_size], None)?;
    let elapsed = start.elapsed().as_secs_f64() * 1000.0;
    volumes.insert(anchor_path.parent().unwrap().to_owned());
    println!("anchor,0,setup,0,{elapsed:.6},{}", volumes.len());
    for index in 0..samples {
        let start = Instant::now();
        let lease = make_lease();
        let path = lease.materialize(&vec![43; payload_size], None)?;
        let elapsed = start.elapsed().as_secs_f64() * 1000.0;
        volumes.insert(path.parent().unwrap().to_owned());
        println!("sequential,{index},join,0,{elapsed:.6},{}", volumes.len());
        if std::fs::read(&path)? != vec![43; payload_size] {
            return Err(io::Error::other("Incorrect overlap payload"));
        }
        let start = Instant::now();
        drop(lease);
        let elapsed = start.elapsed().as_secs_f64() * 1000.0;
        println!("sequential,{index},drop,1,{elapsed:.6},{}", volumes.len());
        if path.exists() || std::fs::read(&anchor_path)? != vec![42; payload_size] {
            return Err(io::Error::other(
                "Dropping an independent lease broke isolation",
            ));
        }
    }
    let barrier = Arc::new(Barrier::new(parallel));
    let threads: Vec<_> = (0..parallel)
        .map(|_| {
            let barrier = barrier.clone();
            std::thread::spawn(move || -> io::Result<_> {
                barrier.wait();
                let start = Instant::now();
                let lease = FileLease::new(std::process::id())
                    .with_keeper_executable(std::env::current_exe()?);
                let path = lease.materialize(&vec![44; payload_size], None)?;
                let elapsed = start.elapsed().as_secs_f64() * 1000.0;
                Ok((lease, path, elapsed))
            })
        })
        .collect();
    let mut live = Vec::new();
    for thread in threads {
        live.push(
            thread
                .join()
                .map_err(|_| io::Error::other("Overlap thread panicked"))??,
        );
    }
    for (_, path, _) in &live {
        if std::fs::read(path)? != vec![44; payload_size] {
            return Err(io::Error::other("Incorrect concurrent payload"));
        }
        volumes.insert(path.parent().unwrap().to_owned());
    }
    for (index, (lease, path, elapsed)) in live.into_iter().enumerate() {
        println!("parallel,{index},join,0,{elapsed:.6},{}", volumes.len());
        let start = Instant::now();
        drop(lease);
        let elapsed = start.elapsed().as_secs_f64() * 1000.0;
        println!("parallel,{index},drop,1,{elapsed:.6},{}", volumes.len());
        if path.exists() || std::fs::read(&anchor_path)? != vec![42; payload_size] {
            return Err(io::Error::other("Concurrent drop broke isolation"));
        }
    }
    let start = Instant::now();
    drop(anchor);
    let elapsed = start.elapsed().as_secs_f64() * 1000.0;
    println!("anchor,0,drop,1,{elapsed:.6},{}", volumes.len());
    if anchor_path.exists() {
        return Err(io::Error::other("Anchor survived its final drop"));
    }
    Ok(())
}
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn main() -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "Overlap timings require Linux or macOS",
    ))
}

//! Synthetic latency probe: cargo run --release --example lifecycle_timings

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn sample(files: usize) -> std::io::Result<Vec<(&'static str, f64)>> {
    use file_lease::FileLease;
    use std::{io, time::Instant};

    let start = Instant::now();
    let lease = FileLease::new(std::process::id()).with_keeper_executable(std::env::current_exe()?);
    let first = lease.materialize(&[42; 256], None)?;
    let mut timings = vec![("setup", start.elapsed().as_secs_f64() * 1000.0)];
    let mut paths = vec![first];
    for _ in 0..files {
        let start = Instant::now();
        paths.push(lease.materialize(&[42; 256], None)?);
        timings.push(("additional_file", start.elapsed().as_secs_f64() * 1000.0));
    }
    // Verify outside the allocation timers, and retain every file until cleanup.
    for path in &paths {
        if std::fs::read(path)? != [42; 256] {
            return Err(io::Error::other("Incorrect synthetic file contents"));
        }
    }
    let start = Instant::now();
    drop(lease);
    timings.push(("cleanup", start.elapsed().as_secs_f64() * 1000.0));
    if paths.iter().any(|path| path.exists()) {
        return Err(io::Error::other("Synthetic file survived lease cleanup"));
    }
    Ok(timings)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn main() -> std::io::Result<()> {
    use std::sync::{Arc, Barrier};

    if let Some(code) = file_lease::maybe_run_keeper() {
        std::process::exit(code);
    }
    let mut samples = 10;
    let mut files = 20;
    let mut parallel = 4;
    let mut arguments = std::env::args().skip(1);
    while let Some(argument) = arguments.next() {
        let destination = match argument.as_str() {
            "--samples" => &mut samples,
            "--files" => &mut files,
            "--parallel" => &mut parallel,
            _ => return Err(std::io::Error::other("Unknown benchmark option")),
        };
        *destination = arguments
            .next()
            .and_then(|value| value.parse::<usize>().ok())
            .filter(|value| (1..=1000).contains(value))
            .ok_or_else(|| std::io::Error::other("Benchmark counts must be between 1 and 1000"))?;
    }
    println!("mode,sample,operation,index,elapsed_ms");
    for index in 0..samples {
        for (operation_index, (operation, ms)) in sample(files)?.into_iter().enumerate() {
            println!("sequential,{index},{operation},{operation_index},{ms:.6}");
        }
    }
    let barrier = Arc::new(Barrier::new(parallel));
    let threads: Vec<_> = (0..parallel)
        .map(|_| {
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                sample(files)
            })
        })
        .collect();
    for (index, thread) in threads.into_iter().enumerate() {
        let timings = thread
            .join()
            .map_err(|_| std::io::Error::other("Latency probe thread panicked"))??;
        for (operation_index, (operation, ms)) in timings.into_iter().enumerate() {
            println!("parallel,{index},{operation},{operation_index},{ms:.6}");
        }
    }
    Ok(())
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn main() -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "File lease timings require Linux or macOS",
    ))
}

//! Reproducible headless harness. Build/run is owned by the integration coordinator.
//! Commands use real allocated files, never `set_len`/sparse fixtures. All latency names
//! say storage viewport, not GUI first paint. OS-cache state is unspecified/warm.
use editor_services::*;
use std::{
    error::Error,
    fs::{File, OpenOptions as FsOpenOptions},
    io::Write,
    path::Path,
    time::{Duration, Instant},
};

type Result<T, E = Box<dyn Error>> = std::result::Result<T, E>;
const FOOTER: &[u8] = b"\n__EDITOR_END_MARKER__\n";
fn main() {
    if let Err(error) = run() {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}
fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("generate") => {
            let path = Path::new(argument(&args, 1)?);
            let bytes = parse_size(argument(&args, 2)?)?;
            let mode = args.get(3).map(String::as_str).unwrap_or("log");
            generate(path, bytes, mode)?;
        }
        Some("bench") => {
            let path = Path::new(argument(&args, 1)?);
            let pattern = args
                .get(2)
                .map(String::as_str)
                .unwrap_or("__ABSENT_BENCH_NEEDLE__");
            let repeats = args
                .get(3)
                .map(|s| s.parse())
                .transpose()?
                .unwrap_or(1usize)
                .clamp(1, 100);
            let blocks = args
                .get(4)
                .map(String::as_str)
                .unwrap_or("64,256,1024,4096")
                .split(',')
                .map(|s| s.parse::<usize>().map(|n| n * 1024))
                .collect::<std::result::Result<Vec<_>, _>>()?;
            environment(path);
            viewport_bench(path, 51)?;
            for block in blocks {
                if !(1..=8 * 1024 * 1024).contains(&block) {
                    return Err("invalid benchmark block size".into());
                }
                for repeat in 0..repeats {
                    let (bytes, elapsed) = sequential_baseline(path, block)?;
                    metric(
                        "sequential_read_at",
                        repeat,
                        block,
                        bytes,
                        bytes,
                        elapsed,
                        0,
                        false,
                    );
                    for algorithm in [
                        SearchAlgorithm::MemchrBaseline,
                        SearchAlgorithm::ByteScanExperiment,
                    ] {
                        let paging = PagingService::default();
                        let doc = paging.open(DocumentId::new(), path)?;
                        let target = SearchTarget::PagedFile {
                            source_gen: doc.source_generation(),
                            handle: doc,
                            encoding: EncodingSpec::Utf8,
                        };
                        let summary = search(target, pattern, block, algorithm)?;
                        metric(
                            match algorithm {
                                SearchAlgorithm::MemchrBaseline => "memchr_file_scan",
                                SearchAlgorithm::ByteScanExperiment => "byte_scan_experiment",
                            },
                            repeat,
                            block,
                            summary.bytes,
                            summary.bytes_read,
                            summary.elapsed,
                            summary.matches,
                            summary.truncated,
                        );
                    }
                }
            }
            cancellation_bench(Some(path))?;
        }
        Some("ram-bench") => {
            let size = args
                .get(1)
                .map(|s| parse_size(s))
                .transpose()?
                .unwrap_or(64 * 1024 * 1024)
                .min(256 * 1024 * 1024) as usize;
            let text = "abcdefgh 中文🙂 log payload\n";
            let rope = Rope::from_str(&text.repeat(size / text.len()));
            println!(
                "kind=ram_snapshot_pipeline includes=bounded_block_copy,kernel no_disk=true bytes={}",
                rope.len()
            );
            for algorithm in [
                SearchAlgorithm::MemchrBaseline,
                SearchAlgorithm::ByteScanExperiment,
            ] {
                for block in [64 * 1024, 256 * 1024, 1024 * 1024, 4 * 1024 * 1024] {
                    let target = SearchTarget::EditableSnapshot {
                        doc_id: DocumentId::new(),
                        rev: Revision(0),
                        rope: rope.clone(),
                    };
                    let summary = search(target, "__ABSENT_BENCH_NEEDLE__", block, algorithm)?;
                    metric(
                        &format!("ram_{algorithm:?}"),
                        0,
                        block,
                        summary.bytes,
                        summary.bytes_read,
                        summary.elapsed,
                        summary.matches,
                        summary.truncated,
                    );
                }
            }
        }
        Some("search") => {
            let path = Path::new(argument(&args, 1)?);
            let pattern = argument(&args, 2)?;
            let paging = PagingService::default();
            let doc = paging.open(DocumentId::new(), path)?;
            let mut request = SearchRequest::new(
                QueryId(1),
                pattern.as_bytes().to_vec(),
                SearchTarget::PagedFile {
                    source_gen: doc.source_generation(),
                    handle: doc,
                    encoding: EncodingSpec::Utf8,
                },
            );
            request.block_bytes = args
                .get(3)
                .map(|s| parse_size(s).map(|n| n as usize))
                .transpose()?
                .unwrap_or(1024 * 1024);
            let task = SearchService::default().start(request)?;
            while let Ok(event) = task.events.recv() {
                match event {
                    SearchEvent::Batch { matches, .. } => {
                        for hit in matches {
                            println!(
                                "{}..{} {:?}",
                                hit.byte_range.start, hit.byte_range.end, hit.preview
                            );
                        }
                    }
                    event => println!("{event:?}"),
                }
            }
        }
        Some("cache-bench") => cache_bench(
            Path::new(argument(&args, 1)?),
            args.get(2).map(|s| s.parse()).transpose()?.unwrap_or(16),
        )?,
        Some("index-bench") => index_bench(Path::new(argument(&args, 1)?))?,
        Some("cancel-bench") => cancellation_bench(args.get(1).map(Path::new))?,
        Some("session-bench") => session_bench(
            Path::new(argument(&args, 1)?),
            args.get(2).map(|s| s.parse()).transpose()?.unwrap_or(100),
        )?,
        Some("session-fixture") => {
            let directory = Path::new(argument(&args, 1)?);
            let views = args.get(2).map(|s| s.parse()).transpose()?.unwrap_or(100);
            let groups = args
                .get(3)
                .map(|s| s.parse())
                .transpose()?
                .unwrap_or(views.min(8));
            std::fs::create_dir_all(directory)?;
            let database = directory.join("recovery.sqlite3");
            create_session_fixture(&database, views, groups)?;
            println!(
                "session_fixture views={views} groups={groups} database={} hydrated_editor_cap=128",
                database.display()
            );
        }
        Some("recovery-bench") => recovery_bench(
            Path::new(argument(&args, 1)?),
            args.get(2)
                .map(|s| parse_size(s))
                .transpose()?
                .unwrap_or(4 * 1024 * 1024) as usize,
            args.get(3).map(|s| s.parse()).transpose()?.unwrap_or(20),
        )?,
        Some("smoke") => smoke(Path::new(argument(&args, 1)?))?,
        Some("crash-recovery") => crash_recovery(Path::new(argument(&args, 1)?))?,
        Some("recover-check") => recover_check(Path::new(argument(&args, 1)?))?,
        Some("crash-save") => crash_save(Path::new(argument(&args, 1)?), argument(&args, 2)?)?,
        _ => {
            println!(
                "editor-services commands:\n  generate PATH SIZE [log|long-line|dense]\n  bench PATH [PATTERN] [REPEATS] [BLOCK_KIB_CSV]\n  ram-bench [SIZE]\n  search PATH PATTERN [BLOCK_SIZE]\n  cache-bench DIRECTORY [FILES=16]\n  index-bench PATH\n  cancel-bench [PATH]\n  session-bench DIRECTORY [TABS=100]\n  smoke DIRECTORY\n  crash-recovery DIRECTORY  # intentionally exits 86 before SQLite commit\n  recover-check DIRECTORY\n  crash-save DIRECTORY [before-write|temp-sync|before-rename|after-rename|recovery-ack]\nSizes: 1GiB, 10GiB, 128MiB, or bytes. Generation refuses to overwrite existing files.\nUse /usr/bin/time -l for peak RSS; retain file sizes, disk identity, build profile and OS-cache state with results.\nExamples: generate .bench-data/one.log 1GiB log; generate .bench-data/ten.log 10GiB log; generate .bench-data/line.log 128MiB long-line; generate .bench-data/dense.log 512MiB dense."
            );
            println!(
                "  recovery-bench DIRECTORY [SIZE=4MiB] [COMMITS=20]  # includes worker replay equality and SQLite FULL commits"
            );
            println!(
                "  session-fixture DIRECTORY [VIEWS=100] [GROUPS=8]  # real 1..1024-view, 1..128-group restart fixture; refuses existing recovery.sqlite3"
            );
        }
    }
    Ok(())
}
fn argument(args: &[String], index: usize) -> Result<&str> {
    args.get(index)
        .map(String::as_str)
        .ok_or_else(|| "missing command argument; run without arguments for usage".into())
}
fn parse_size(value: &str) -> Result<u64> {
    let (number, multiplier) = if let Some(n) = value.strip_suffix("GiB") {
        (n, 1024u64.pow(3))
    } else if let Some(n) = value.strip_suffix("MiB") {
        (n, 1024u64.pow(2))
    } else if let Some(n) = value.strip_suffix("KiB") {
        (n, 1024u64)
    } else {
        (value, 1)
    };
    number
        .parse::<u64>()?
        .checked_mul(multiplier)
        .ok_or_else(|| "size overflow".into())
}
fn environment(path: &Path) {
    println!(
        "environment os={} arch={} profile={} path={} os_cache=unspecified_or_warm",
        std::env::consts::OS,
        std::env::consts::ARCH,
        if cfg!(debug_assertions) {
            "debug_NOT_PERFORMANCE_EVIDENCE"
        } else {
            "release"
        },
        path.display()
    );
    for (program, args) in [
        ("sw_vers", vec![]),
        (
            "sysctl",
            vec!["-n", "hw.model", "hw.memsize", "machdep.cpu.brand_string"],
        ),
    ] {
        if let Ok(output) = std::process::Command::new(program).args(args).output() {
            if output.status.success() {
                println!(
                    "{}: {}",
                    program,
                    String::from_utf8_lossy(&output.stdout).trim()
                );
            }
        }
    }
    println!(
        "storage=record_actual_volume_interface_for_this_path no_cold_cache_or_NVMe_claim=true"
    );
}
fn require_space(path: &Path, bytes: u64) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let path = std::ffi::CString::new(path.as_os_str().as_bytes())?;
        let mut info = std::mem::MaybeUninit::<libc::statvfs>::uninit();
        // SAFETY: path is terminated, and statvfs initializes info on a zero result.
        if unsafe { libc::statvfs(path.as_ptr(), info.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let info = unsafe { info.assume_init() };
        let available = info.f_bavail as u128 * info.f_frsize as u128;
        if available < bytes as u128 + 512 * 1024 * 1024 {
            return Err(format!(
                "insufficient free space: available={available}, requested={bytes}, reserve=512MiB"
            )
            .into());
        }
    }
    Ok(())
}
fn generate(path: &Path, bytes: u64, mode: &str) -> Result<()> {
    if bytes < 64 {
        return Err("generated fixture must be at least 64 bytes".into());
    }
    if !matches!(mode, "log" | "long-line" | "dense") {
        return Err("generation mode must be log, long-line or dense".into());
    }
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    require_space(parent, bytes)?;
    let mut file = FsOpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    let mut block = Vec::with_capacity(1024 * 1024);
    match mode {
        "log" => {
            let record = "2026-11-06T12:34:56Z INFO request=8f4a message=中文🙂 payload=abcdefghijklmnopqrstuvwxyz0123456789\r\n".as_bytes();
            while block.len() + record.len() <= 1024 * 1024 {
                block.extend_from_slice(record);
            }
        }
        "long-line" => block.resize(1024 * 1024, b'x'),
        "dense" => block.resize(1024 * 1024, b'\n'),
        _ => return Err("generation mode must be log, long-line or dense".into()),
    }
    let started = Instant::now();
    // No seek/set_len: every byte is physically submitted to write(2).
    let content_bytes = if mode == "log" { bytes - 64 } else { bytes };
    let mut written = 0u64;
    while written < content_bytes {
        let mut n = (content_bytes - written).min(block.len() as u64) as usize;
        if mode == "log" && n < block.len() {
            while n > 0 && block[n] & 0xc0 == 0x80 {
                n -= 1;
            }
            if n == 0 {
                file.write_all(b" ")?;
                written += 1;
                continue;
            }
        }
        file.write_all(&block[..n])?;
        written += n as u64;
    }
    if mode == "log" {
        file.write_all(FOOTER)?;
        file.write_all(&[b' '; 64][..64 - FOOTER.len()])?;
    }
    file.sync_all()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        println!(
            "generated path={} mode={} logical_bytes={} allocated_bytes={} seconds={:.6}",
            path.display(),
            mode,
            bytes,
            file.metadata()?.blocks() * 512,
            started.elapsed().as_secs_f64()
        );
    }
    #[cfg(not(unix))]
    println!(
        "generated path={} mode={} logical_bytes={} seconds={:.6}",
        path.display(),
        mode,
        bytes,
        started.elapsed().as_secs_f64()
    );
    Ok(())
}
fn sequential_baseline(path: &Path, block: usize) -> Result<(u64, Duration)> {
    let file = File::open(path)?;
    let mut buffer = vec![0; block];
    let mut offset = 0;
    let started = Instant::now();
    loop {
        #[cfg(unix)]
        let n = {
            use std::os::unix::fs::FileExt;
            file.read_at(&mut buffer, offset)?
        };
        #[cfg(windows)]
        let n = {
            use std::os::windows::fs::FileExt;
            file.seek_read(&mut buffer, offset)?
        };
        if n == 0 {
            break;
        }
        std::hint::black_box(&buffer[..n]);
        offset += n as u64;
    }
    Ok((offset, started.elapsed()))
}
struct SearchSummary {
    bytes: u64,
    bytes_read: u64,
    elapsed: Duration,
    matches: usize,
    truncated: bool,
}
fn search(
    target: SearchTarget,
    pattern: &str,
    block: usize,
    algorithm: SearchAlgorithm,
) -> Result<SearchSummary> {
    let mut request = SearchRequest::new(QueryId(1), pattern.as_bytes().to_vec(), target);
    request.block_bytes = block;
    request.algorithm = algorithm;
    let task = admit_recovery(|| SearchService::default().start(request.clone()))?;
    while let Ok(event) = task.events.recv() {
        match event {
            SearchEvent::Done {
                bytes_scanned,
                bytes_read,
                elapsed_ns,
                total_emitted,
                truncated,
                ..
            } => {
                return Ok(SearchSummary {
                    bytes: bytes_scanned,
                    bytes_read,
                    elapsed: Duration::from_nanos(elapsed_ns),
                    matches: total_emitted,
                    truncated,
                });
            }
            SearchEvent::Failed { error, .. } => return Err(error.into()),
            SearchEvent::Invalidated { .. } => return Err("benchmark source changed".into()),
            SearchEvent::Cancelled { .. } => return Err("benchmark search cancelled".into()),
            SearchEvent::Batch { matches, .. } => {
                std::hint::black_box(matches);
            }
        }
    }
    Err("search channel closed without terminal event".into())
}
fn metric(
    kind: &str,
    repeat: usize,
    block: usize,
    bytes: u64,
    bytes_read: u64,
    elapsed: Duration,
    matches: usize,
    truncated: bool,
) {
    if truncated {
        println!(
            "metric={kind} repeat={repeat} block_bytes={block} bytes_scanned={bytes} bytes_read={bytes_read} seconds={:.9} GiB_per_sec=omitted_capped matches={matches} truncated=true complete_throughput_valid=false",
            elapsed.as_secs_f64()
        );
    } else {
        println!(
            "metric={kind} repeat={repeat} block_bytes={block} bytes_scanned={bytes} bytes_read={bytes_read} seconds={:.9} GiB_per_sec={:.6} matches={matches} truncated=false complete_throughput_valid=true",
            elapsed.as_secs_f64(),
            bytes as f64 / 1024f64.powi(3) / elapsed.as_secs_f64().max(1e-9)
        );
    }
}
fn viewport_bench(path: &Path, repeats: usize) -> Result<()> {
    let mut samples = Vec::with_capacity(repeats);
    let paging = PagingService::default();
    for _ in 0..repeats {
        let started = Instant::now();
        let doc = paging.open(DocumentId::new(), path)?;
        let view = doc.viewport(
            ViewportRequest {
                source_gen: doc.source_generation(),
                anchor: ViewportAnchor::ByteOffset(0),
                max_bytes: 65_536,
            },
            &CancelToken::new(),
        )?;
        samples.push(started.elapsed().as_nanos() as u64);
        if view.text.len() > 65_536 {
            return Err("viewport exceeded decode cap".into());
        }
        std::hint::black_box(view);
    }
    samples.sort_unstable();
    println!(
        "metric=storage_viewport_open repeats={repeats} p50_us={:.3} p95_us={:.3} page_cache_each_handle=fresh OS_cache=unspecified_or_warm",
        samples[repeats / 2] as f64 / 1000.0,
        samples[(repeats * 95 / 100).min(repeats - 1)] as f64 / 1000.0
    );
    Ok(())
}
fn cache_bench(directory: &Path, count: usize) -> Result<()> {
    if !(16..=128).contains(&count) {
        return Err("cache benchmark requires 16..128 files".into());
    }
    std::fs::create_dir_all(directory)?;
    environment(directory);
    let paging = PagingService::default();
    let mut docs = Vec::with_capacity(count);
    for n in 0..count {
        let path = directory.join(format!("cache-{n:03}.log"));
        if !path.exists() {
            generate(&path, 8 * 1024 * 1024, "log")?;
        }
        if std::fs::metadata(&path)?.len() != 8 * 1024 * 1024 {
            return Err(format!("cache fixture has an unexpected size: {}", path.display()).into());
        }
        docs.push(paging.open(DocumentId::new(), path)?);
    }
    let mut peak = 0;
    let mut samples = Vec::with_capacity(count * 128);
    let mut decoded_bytes = 0u64;
    let started = Instant::now();
    std::thread::scope(|scope| -> Result<()> {
        let mut indexers = Vec::new();
        for worker in 0..4 {
            let docs = &docs;
            indexers.push(
                scope.spawn(move || -> std::result::Result<(), ServiceError> {
                    for doc in docs.iter().skip(worker).step_by(4) {
                        let generation = doc.source_generation();
                        loop {
                            match doc.index_step(generation, &CancelToken::new()) {
                                Ok(IndexStatus::Complete { .. }) => break,
                                Ok(_) => {}
                                Err(error) if error.kind == ErrorKind::Busy => {
                                    std::thread::sleep(Duration::from_millis(1))
                                }
                                Err(error) => return Err(error),
                            }
                        }
                    }
                    Ok(())
                }),
            );
        }
        for offset in (0..8 * 1024 * 1024).step_by(65_536) {
            for doc in &docs {
                let before = Instant::now();
                let view = doc.viewport(
                    ViewportRequest {
                        source_gen: doc.source_generation(),
                        anchor: ViewportAnchor::ByteOffset(offset),
                        max_bytes: 65_536,
                    },
                    &CancelToken::new(),
                )?;
                samples.push(before.elapsed().as_nanos() as u64);
                decoded_bytes += view.text.len() as u64;
                std::hint::black_box(view);
            }
            let stats = paging.stats();
            peak = peak.max(stats.cached_bytes);
            if stats.cached_bytes > stats.cache_budget
                || stats.index_bytes > stats.index_budget
                || stats.peak_index_jobs > 2
                || stats.peak_open_jobs > 2
            {
                return Err("global budget or concurrency quota exceeded".into());
            }
        }
        for indexer in indexers {
            indexer
                .join()
                .map_err(|_| "index benchmark worker panicked")??;
        }
        Ok(())
    })?;
    samples.sort_unstable();
    println!(
        "metric=multifile_storage_viewport_with_indexers files={count} samples={} p50_us={:.3} p95_us={:.3} total_seconds={:.9} decoded_GiB_per_sec={:.6} peak_cached_bytes={peak} stats={:?}",
        samples.len(),
        samples[samples.len() / 2] as f64 / 1000.0,
        samples[samples.len() * 95 / 100] as f64 / 1000.0,
        started.elapsed().as_secs_f64(),
        decoded_bytes as f64 / 1024f64.powi(3) / started.elapsed().as_secs_f64(),
        paging.stats()
    );
    let before = Instant::now();
    let mut bytes = 0;
    for doc in &docs {
        bytes += sequential_baseline(doc.path(), 1024 * 1024)?.0;
    }
    metric(
        "multifile_sequential_read_at",
        0,
        1024 * 1024,
        bytes,
        bytes,
        before.elapsed(),
        0,
        false,
    );
    for algorithm in [
        SearchAlgorithm::MemchrBaseline,
        SearchAlgorithm::ByteScanExperiment,
    ] {
        let before = Instant::now();
        let mut bytes_scanned = 0;
        let mut bytes_read = 0;
        for batch in docs.chunks(4) {
            let mut tasks = Vec::with_capacity(batch.len());
            for doc in batch {
                let mut request = SearchRequest::new(
                    QueryId(1),
                    b"__ABSENT_BENCH_NEEDLE__".to_vec(),
                    SearchTarget::PagedFile {
                        source_gen: doc.source_generation(),
                        handle: doc.clone(),
                        encoding: EncodingSpec::Utf8,
                    },
                );
                request.algorithm = algorithm;
                tasks.push(admit_recovery(|| {
                    SearchService::default().start(request.clone())
                })?);
            }
            for task in tasks {
                loop {
                    match task.events.recv_timeout(Duration::from_secs(60))? {
                        SearchEvent::Done {
                            bytes_scanned: scanned,
                            bytes_read: read,
                            truncated: false,
                            ..
                        } => {
                            bytes_scanned += scanned;
                            bytes_read += read;
                            break;
                        }
                        SearchEvent::Batch { .. } => {}
                        event => return Err(format!("multifile search failed: {event:?}").into()),
                    }
                }
            }
        }
        metric(
            &format!("multifile_{algorithm:?}_worker_limit4"),
            0,
            1024 * 1024,
            bytes_scanned,
            bytes_read,
            before.elapsed(),
            0,
            false,
        );
    }
    drop(docs);
    println!("metric=multifile_after_close stats={:?}", paging.stats());
    Ok(())
}
fn index_bench(path: &Path) -> Result<()> {
    let paging = PagingService::default();
    let doc = paging.open(DocumentId::new(), path)?;
    let start = Instant::now();
    loop {
        let status =
            admit_recovery(|| doc.index_step(doc.source_generation(), &CancelToken::new()))?;
        let stats = paging.stats();
        if stats.index_bytes > stats.index_budget {
            return Err("index budget exceeded".into());
        }
        if let IndexStatus::Complete {
            total_bytes,
            total_lines,
        } = status
        {
            println!(
                "metric=line_index bytes={total_bytes} lines={total_lines} seconds={:.9} stats={stats:?}",
                start.elapsed().as_secs_f64()
            );
            break;
        }
    }
    Ok(())
}
fn cancellation_bench(path: Option<&Path>) -> Result<()> {
    for service in [SearchService::with_limit(1), SearchService::default()] {
        let id = DocumentId::new();
        let rope = Rope::from_str(&format!("{}needle-final", "a".repeat(2 * 1024 * 1024)));
        let mut latest = None;
        let started = Instant::now();
        for query in 1..=20 {
            let pattern = if query == 20 { "needle-final" } else { "a" };
            latest = Some(service.start(SearchRequest::new(
                QueryId(query),
                pattern.as_bytes().to_vec(),
                SearchTarget::EditableSnapshot {
                    doc_id: id,
                    rev: Revision(1),
                    rope: rope.clone(),
                },
            ))?);
        }
        let task = latest.ok_or("missing replacement task")?;
        loop {
            match task.events.recv_timeout(Duration::from_secs(10))? {
                SearchEvent::Batch {
                    query_id: QueryId(20),
                    ..
                } => {}
                SearchEvent::Done {
                    query_id: QueryId(20),
                    total_emitted: 1,
                    truncated: false,
                    ..
                } => break,
                event => {
                    return Err(
                        format!("latest query was dropped or contaminated: {event:?}").into(),
                    );
                }
            }
        }
        let settle = Instant::now();
        while service.active_tasks() != 0 && settle.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(1));
        }
        if service.active_tasks() != 0 {
            return Err("replacement search slot did not settle".into());
        }
        println!(
            "metric=rapid_query_replacement accepted=20 final_query=20 final_matches=1 total_us={}",
            started.elapsed().as_micros()
        );
    }
    if let Some(path) = path {
        let doc = PagingService::default().open(DocumentId::new(), path)?;
        let request = SearchRequest::new(
            QueryId(9),
            b"__ABSENT_BENCH_NEEDLE__".to_vec(),
            SearchTarget::PagedFile {
                source_gen: doc.source_generation(),
                handle: doc,
                encoding: EncodingSpec::Utf8,
            },
        );
        let service = SearchService::with_limit(1);
        let task = service.start(request)?;
        std::thread::sleep(Duration::from_millis(20));
        let already_done = service.active_tasks() == 0;
        let start = Instant::now();
        task.cancel.cancel();
        while service.active_tasks() != 0 && start.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(1));
        }
        println!(
            "metric=file_cancel elapsed_us={} completed_before_cancel={already_done} active={}",
            start.elapsed().as_micros(),
            service.active_tasks()
        );
        if service.active_tasks() != 0 {
            return Err("file cancellation exceeded 10s".into());
        }
    }
    let service = SearchService::with_limit(1);
    let request = SearchRequest::new(
        QueryId(10),
        b"a".to_vec(),
        SearchTarget::EditableSnapshot {
            doc_id: DocumentId::new(),
            rev: Revision(1),
            rope: Rope::from_str(&"a".repeat(2 * 1024 * 1024)),
        },
    );
    let task = service.start(request)?;
    let wait = Instant::now();
    while task.events.len() < 8 && wait.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(1));
    }
    if task.events.len() != 8 {
        return Err("did not exercise a full result queue".into());
    }
    let start = Instant::now();
    task.cancel.cancel();
    // Intentionally do not drain: cancellation must break send backpressure itself.
    while service.active_tasks() != 0 && start.elapsed() < Duration::from_secs(10) {
        std::thread::sleep(Duration::from_millis(1));
    }
    println!(
        "metric=full_result_queue_cancel queue_len={} elapsed_us={} active={}",
        task.events.len(),
        start.elapsed().as_micros(),
        service.active_tasks()
    );
    if service.active_tasks() != 0 {
        return Err("full-queue cancellation exceeded 10s".into());
    }
    Ok(())
}
fn ack<T>(reply: Reply<T>) -> Result<T> {
    Ok(reply.recv_timeout(Duration::from_secs(30))??)
}
fn admit_recovery<T>(
    mut operation: impl FnMut() -> std::result::Result<T, ServiceError>,
) -> Result<T> {
    let started = Instant::now();
    loop {
        match operation() {
            Ok(value) => return Ok(value),
            Err(error)
                if error.kind == ErrorKind::Busy && started.elapsed() < Duration::from_secs(30) =>
            {
                // CLI-only blocking retry. Interactive consumers schedule retries instead.
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(error) => return Err(error.into()),
        }
    }
}
fn commit(doc_id: DocumentId, revision: u64, text: &str) -> RecoveryCommit {
    RecoveryCommit {
        doc_id,
        lifecycle_gen: LifecycleGen(1),
        base_rev: Revision(revision.saturating_sub(1)),
        new_rev: Revision(revision),
        rope: Rope::from_str(text),
        deltas: Vec::new(),
        meta: DocMeta {
            title: "Untitled".into(),
            ..Default::default()
        },
    }
}
fn create_session_fixture(database: &Path, views: usize, group_count: usize) -> Result<()> {
    if views == 0 || views > MAX_SESSION_VIEWS {
        return Err("session fixture accepts 1..1024 metadata views (the separate hydrated-editor cap remains 128)".into());
    }
    if !(1..=MAX_CUSTOM_GROUPS).contains(&group_count) {
        return Err("session fixture accepts 1..128 groups".into());
    }
    if database.exists() {
        return Err("refusing to overwrite an existing recovery database".into());
    }
    let service = RecoveryHandle::start(database)?;
    let initial = ack(service.request_manifest()?)?;
    let mut layout = initial.layout;
    let mut changes = Vec::with_capacity(views);
    let mut durable = std::collections::HashSet::new();
    let text = "draft 中文🙂\n".repeat(4096);
    let mut groups = Vec::new();
    for n in 0..group_count {
        let group = layout
            .apply_group_command(GroupCommand::Create {
                name: format!("Fixture group {n} 中文"),
                color: GroupColor::ALL[n % 8 + 1],
                before: None,
                initial_view: None,
            })?
            .created_group
            .ok_or("missing fixture group")?;
        layout.apply_group_command(GroupCommand::SetCollapsed {
            group,
            collapsed: n % 2 == 0,
        })?;
        groups.push(group);
    }
    for n in 0..views {
        let id = DocumentId::new();
        let key = DocumentKey {
            doc_id: id,
            lifecycle_gen: LifecycleGen(1),
        };
        let mut job = commit(id, 1, &text);
        job.meta.title = format!("Draft {n}");
        let cursor_byte = (n * "draft 中文🙂\n".len()) as u64;
        layout.insert_view(
            LayoutView {
                id: ViewId::new(),
                document: key,
                group_id: groups[n % groups.len()],
                view: ViewState {
                    cursor_byte,
                    scroll_byte: cursor_byte,
                },
            },
            None,
            n == views / 2,
        )?;
        changes.push(DocumentChange::Create(DocumentMetaRecord {
            key,
            meta: job.meta.clone(),
        }));
        admit_recovery(|| service.try_commit(job.clone()))?;
        if n % 32 == 31 || n + 1 == views {
            ack(admit_recovery(|| service.flush())?)?;
            for event in service.events().try_iter() {
                match event {
                    RecoveryEvent::DurableAck {
                        doc_id,
                        durable_rev: Revision(1),
                        ..
                    } => {
                        durable.insert(doc_id);
                    }
                    RecoveryEvent::StorageError { error, .. } => return Err(error.into()),
                    event => {
                        return Err(format!("unexpected fixture acknowledgment: {event:?}").into());
                    }
                }
            }
        }
    }
    if durable.len() != views
        || !layout
            .views
            .values()
            .all(|view| durable.contains(&view.document.doc_id))
    {
        return Err("not every fixture document was durably acknowledged".into());
    }
    ack(admit_recovery(|| {
        service.write_session(SessionWrite {
            sequence: LayoutSeq(1),
            expected_revision: initial.layout_revision,
            layout: LayoutPayload::Structural(layout.clone()),
            document_changes: changes.clone(),
            preserve_source_layout: None,
        })
    })?)?;
    ack(admit_recovery(|| service.shutdown())?)?;
    Ok(())
}
fn session_bench(directory: &Path, views: usize) -> Result<()> {
    std::fs::create_dir_all(directory)?;
    let database = directory.join(format!("session-{}.sqlite3", uuid::Uuid::new_v4()));
    create_session_fixture(&database, views, views.min(8))?;
    let started = Instant::now();
    let service = RecoveryHandle::start(&database)?;
    let worker_open_us = started.elapsed().as_micros();
    let started = Instant::now();
    let manifest = ack(service.request_manifest()?)?;
    let metadata_us = started.elapsed().as_micros();
    if manifest.layout.views.len() != views || manifest.documents.len() != views {
        return Err("restarted metadata fixture lost views".into());
    }
    let active = manifest
        .layout
        .target(manifest.layout.pane().active_view.ok_or("no active view")?)
        .ok_or("active identity missing")?;
    let started = Instant::now();
    let draft = ack(service.request_hydrate(active.document, CancelToken::new())?)?;
    if draft.rope != "draft 中文🙂\n".repeat(4096) {
        return Err("restarted active draft differs".into());
    }
    println!(
        "metric=lazy_session tabs={} views={} worker_open_us={worker_open_us} metadata_us={metadata_us} active_hydrate_us={} hydrated_documents=1 active_bytes={} inactive_hydrated=0 database={}",
        manifest.layout.views.len(),
        manifest.layout.views.len(),
        started.elapsed().as_micros(),
        draft.rope.len(),
        database.display()
    );
    ack(admit_recovery(|| service.shutdown())?)?;
    Ok(())
}
fn recovery_bench(directory: &Path, size: usize, commits: usize) -> Result<()> {
    if size == 0 || size > 64 * 1024 * 1024 || !(1..=1000).contains(&commits) {
        return Err("recovery benchmark requires 1B..64MiB and 1..1000 commits".into());
    }
    std::fs::create_dir_all(directory)?;
    let service =
        RecoveryHandle::start(directory.join(format!("delta-{}.sqlite3", uuid::Uuid::new_v4())))?;
    let id = DocumentId::new();
    let mut rope = Rope::from_str(&"x".repeat(size));
    let baseline = RecoveryCommit {
        doc_id: id,
        lifecycle_gen: LifecycleGen(1),
        base_rev: Revision(0),
        new_rev: Revision(1),
        rope: rope.clone(),
        deltas: Vec::new(),
        meta: DocMeta::default(),
    };
    admit_recovery(|| service.try_commit(baseline.clone()))?;
    ack(admit_recovery(|| service.flush())?)?;
    let mut samples = Vec::with_capacity(commits);
    let mut equality_ns = 0u128;
    for number in 0..commits {
        let previous = rope.clone();
        let replacement = if number % 2 == 0 { "a" } else { "b" };
        rope.remove(size - 1..size);
        rope.insert(size - 1, replacement);
        let mut replayed = previous;
        replayed.remove(size - 1..size);
        replayed.insert(size - 1, replacement);
        let before = Instant::now();
        if !std::hint::black_box(replayed == rope) {
            return Err("equivalent replay comparison failed".into());
        }
        equality_ns += before.elapsed().as_nanos();
        let job = RecoveryCommit {
            doc_id: id,
            lifecycle_gen: LifecycleGen(1),
            base_rev: Revision(number as u64 + 1),
            new_rev: Revision(number as u64 + 2),
            rope: rope.clone(),
            deltas: vec![TextDelta {
                byte_range: (size - 1) as u64..size as u64,
                replacement: replacement.into(),
            }],
            meta: DocMeta::default(),
        };
        let before = Instant::now();
        admit_recovery(|| service.try_commit(job.clone()))?;
        ack(admit_recovery(|| service.flush())?)?;
        samples.push(before.elapsed().as_nanos() as u64);
    }
    let draft = ack(service.request_hydrate(
        DocumentKey {
            doc_id: id,
            lifecycle_gen: LifecycleGen(1),
        },
        CancelToken::new(),
    )?)?;
    if draft.rope != rope {
        return Err("recovery benchmark final draft mismatch".into());
    }
    samples.sort_unstable();
    println!(
        "metric=delta_recovery_worker bytes={size} commits={commits} commit_p50_us={:.3} commit_p95_us={:.3} equivalent_replay_equality_total_ms={:.6} worker_equality_and_sqlite_FULL=included ui_full_text_work=false",
        samples[commits / 2] as f64 / 1000.0,
        samples[(commits * 95 / 100).min(commits - 1)] as f64 / 1000.0,
        equality_ns as f64 / 1_000_000.0
    );
    ack(admit_recovery(|| service.shutdown())?)?;
    Ok(())
}
fn smoke(directory: &Path) -> Result<()> {
    let directory = directory.join(format!("smoke-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&directory)?;
    let path = directory.join("sample.txt");
    std::fs::write(&path, b"\xef\xbb\xbfhello\r\nworld\r\n")?;
    let id = DocumentId::new();
    let opened = admit_recovery(|| {
        open_document(
            id,
            &path,
            &PagingService::default(),
            OpenOptions::default(),
            &CancelToken::new(),
        )
    })?;
    let (rope, encoding, ending, original) = match opened {
        OpenOutcome::Editable {
            rope,
            encoding,
            line_ending,
            fingerprint,
            ..
        } => (rope, encoding, line_ending, fingerprint),
        _ => return Err("small UTF-8 sample unexpectedly paged".into()),
    };
    let faults = FaultInjector::default();
    let recovery =
        RecoveryHandle::start_with_faults(directory.join("recovery.sqlite3"), faults.clone())?;
    let mut first = commit(id, 1, "draft 中文🙂");
    first.meta.path = Some(path.clone());
    first.meta.origin_fingerprint = Some(original.clone());
    admit_recovery(|| recovery.try_commit(first.clone()))?;
    ack(admit_recovery(|| recovery.flush())?)?;
    let mut next = commit(id, 2, "draft 中文🙂!");
    next.deltas = vec![TextDelta {
        byte_range: 16..16,
        replacement: "!".into(),
    }];
    // The byte coordinate is computed from the same pre-edit text, not character count.
    next.deltas[0].byte_range = ("draft 中文🙂".len() as u64)..("draft 中文🙂".len() as u64);
    admit_recovery(|| recovery.try_commit(next.clone()))?;
    ack(admit_recovery(|| recovery.flush())?)?;
    let draft = ack(recovery.request_hydrate(
        DocumentKey {
            doc_id: id,
            lifecycle_gen: LifecycleGen(1),
        },
        CancelToken::new(),
    )?)?;
    if draft.rope != "draft 中文🙂!" {
        return Err("delta recovery content mismatch".into());
    }
    let request = SaveRequest {
        doc_id: id,
        lifecycle_gen: LifecycleGen(1),
        captured_rev: Revision(2),
        rope,
        target_path: path.clone(),
        encoding,
        line_ending: ending,
        expected_fingerprint: Some(original),
        force_overwrite: false,
    };
    faults.arm(FaultPoint::SaveAfterTempSync);
    if !matches!(
        save_document(&request, Some(&recovery), &faults),
        SaveOutcome::Failed { .. }
    ) {
        return Err("save fault was not exercised".into());
    }
    if std::fs::read(&path)? != b"\xef\xbb\xbfhello\r\nworld\r\n" {
        return Err("failed save changed original".into());
    }
    match save_document(&request, Some(&recovery), &faults) {
        SaveOutcome::Saved { .. } | SaveOutcome::SavedWithRecoveryWarning { .. } => {}
        outcome => return Err(format!("save smoke failed: {outcome:?}").into()),
    }
    let target = SearchTarget::EditableSnapshot {
        doc_id: id,
        rev: Revision(3),
        rope: Rope::from_str("aaaaa中文🙂\r\n中文🙂"),
    };
    for size in [1, 2, 3, 7, 64] {
        let result = search(target.clone(), "aaa", size, SearchAlgorithm::MemchrBaseline)?;
        if result.matches != 1 || result.truncated {
            return Err("non-overlapping cross-block search mismatch".into());
        }
    }
    let manifest = ack(recovery.request_manifest()?)?;
    let layout = manifest.layout;
    let layout_ack = ack(recovery.write_session(SessionWrite {
        sequence: LayoutSeq(1),
        expected_revision: manifest.layout_revision,
        layout: LayoutPayload::Structural(layout.clone()),
        document_changes: Vec::new(),
        preserve_source_layout: None,
    })?)?;
    let target = layout
        .views
        .values()
        .find(|v| v.document.doc_id == id)
        .and_then(|v| layout.target(v.id))
        .ok_or("smoke draft missing from restored layout")?;
    let mut after = layout.clone();
    after.remove_views(&[target])?;
    ack(recovery.close_views(CloseBatch {
        sequence: LayoutSeq(2),
        expected_revision: layout_ack.revision,
        entries: vec![CloseEntry {
            target,
            disposition: CloseDisposition::Discard {
                tombstone_gen: LifecycleGen(2),
            },
            required_durable_rev: Some(Revision(2)),
            expected_format: (draft.meta.encoding, draft.meta.line_ending),
            view: ViewState::default(),
            expected_identity: None,
        }],
        after,
        empty_pane_document: None,
    })?)?;
    let late = commit(id, 3, "late stale checkpoint");
    admit_recovery(|| recovery.try_commit(late.clone()))?;
    ack(admit_recovery(|| recovery.flush())?)?;
    if ack(recovery.request_hydrate(
        DocumentKey {
            doc_id: id,
            lifecycle_gen: LifecycleGen(1),
        },
        CancelToken::new(),
    )?)
    .is_ok()
    {
        return Err("discard resurrected a draft".into());
    }
    ack(admit_recovery(|| recovery.shutdown())?)?;
    println!(
        "smoke_passed directory={} scenarios=open_bom_crlf,delta_recovery,save_temp_fault,atomic_save,cross_block_nonoverlap,discard_guard",
        directory.display()
    );
    std::fs::remove_dir_all(directory)?;
    Ok(())
}
fn crash_recovery(directory: &Path) -> Result<()> {
    std::fs::create_dir_all(directory)?;
    let faults = FaultInjector::default();
    let service =
        RecoveryHandle::start_with_faults(directory.join("crash.sqlite3"), faults.clone())?;
    let id = DocumentId(0xc0de);
    let baseline = commit(id, 1, "durable baseline 中文🙂");
    admit_recovery(|| service.try_commit(baseline.clone()))?;
    ack(admit_recovery(|| service.flush())?)?;
    println!("durable_revision=1 next_revision=2 exit_stage=RecoveryBeforeCommit expected_exit=86");
    std::io::stdout().flush()?;
    faults.arm_process_exit(FaultPoint::RecoveryBeforeCommit);
    let uncommitted = commit(id, 2, "must not recover uncommitted text");
    admit_recovery(|| service.try_commit(uncommitted.clone()))?;
    ack(admit_recovery(|| service.flush())?)?;
    Err("crash hook did not exit".into())
}
fn recover_check(directory: &Path) -> Result<()> {
    let service = RecoveryHandle::start(directory.join("crash.sqlite3"))?;
    let draft = ack(service.request_hydrate(
        DocumentKey {
            doc_id: DocumentId(0xc0de),
            lifecycle_gen: LifecycleGen(1),
        },
        CancelToken::new(),
    )?)?;
    if draft.durable_rev != Revision(1) || draft.rope != "durable baseline 中文🙂" {
        return Err(
            "recovery after process exit did not preserve the last committed revision".into(),
        );
    }
    println!(
        "crash_recovery_passed durable_revision={} text={}",
        draft.durable_rev.0, draft.rope
    );
    ack(admit_recovery(|| service.shutdown())?)?;
    Ok(())
}
fn crash_save(directory: &Path, stage: &str) -> Result<()> {
    std::fs::create_dir_all(directory)?;
    let point = match stage {
        "before-write" => FaultPoint::SaveBeforeWrite,
        "temp-sync" => FaultPoint::SaveAfterTempSync,
        "before-rename" => FaultPoint::SaveBeforeRename,
        "after-rename" => FaultPoint::SaveAfterRename,
        "recovery-ack" => FaultPoint::SaveBeforeRecoveryAck,
        _ => return Err("unknown save fault stage".into()),
    };
    let path = directory.join("crash-save.txt");
    let mut file = FsOpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;
    file.write_all(b"original\n")?;
    file.sync_all()?;
    drop(file);
    let service = RecoveryHandle::start(directory.join("save.sqlite3"))?;
    let id = DocumentId(0x5a7e);
    let baseline = commit(id, 1, "captured new text\n");
    admit_recovery(|| service.try_commit(baseline.clone()))?;
    ack(admit_recovery(|| service.flush())?)?;
    let request = SaveRequest {
        doc_id: id,
        lifecycle_gen: LifecycleGen(1),
        captured_rev: Revision(1),
        rope: Rope::from_str("captured new text\n"),
        target_path: path.clone(),
        encoding: EncodingSpec::Utf8,
        line_ending: LineEnding::Lf,
        expected_fingerprint: Some(FileFingerprint::of_path(&path)?),
        force_overwrite: false,
    };
    let faults = FaultInjector::default();
    faults.arm_process_exit(point);
    println!(
        "save_process_exit stage={point:?} expected_exit=86 target={}",
        path.display()
    );
    std::io::stdout().flush()?;
    let result = save_document(&request, Some(&service), &faults);
    Err(format!("crash hook did not exit: {result:?}").into())
}

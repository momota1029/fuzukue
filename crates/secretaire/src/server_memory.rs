//! Bound glibc arena growth and give idle allocations back to Linux.

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::App;

pub(crate) fn configure_allocator() {
    // Keep administrator overrides from the environment, including GLIBC_TUNABLES.
    if std::env::var_os("MALLOC_ARENA_MAX").is_none()
        && !std::env::var("GLIBC_TUNABLES").unwrap_or_default().split(':')
            .any(|setting| setting.starts_with("glibc.malloc.arena_max="))
    {
        unsafe { libc::mallopt(libc::M_ARENA_MAX, 4); }
    }
}

pub(crate) fn start_trimming(app: &Arc<App>) {
    let app = Arc::downgrade(app);
    tokio::spawn(async move {
        let mut ticks = 0;
        loop {
            tokio::time::sleep(Duration::from_secs(60)).await;
            let Some(app) = app.upgrade() else { break };
            ticks = (ticks + 1) % 5;
            let _ = tokio::task::spawn_blocking(move || {
                if ticks == 0 && tracing::enabled!(tracing::Level::INFO) {
                    report_and_trim(&app);
                } else {
                    // SAFETY: malloc_trim is thread safe.
                    unsafe { libc::malloc_trim(0) };
                }
            }).await;
        }
    });
}

fn rss_kib() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let rss = status.lines().find_map(|line| line.strip_prefix("VmRSS:"))?;
    rss.split_whitespace().next()?.parse().ok()
}

#[derive(Debug, PartialEq, Eq)]
struct Heap {
    arenas: usize,
    used_bytes: u64,
    free_bytes: u64,
    mmap_bytes: u64,
}

impl Heap {
    // Global totals follow the per-arena heaps. Fast and ordinary free chunks
    // are counted separately. Thread-cache chunks can appear as in use.
    fn from_xml(xml: &str) -> Option<Self> {
        let doc = roxmltree::Document::parse(xml).ok()?;
        let root = doc.root_element();
        if !root.has_tag_name("malloc") {
            return None;
        }
        let metric = |tag, kind, attr| -> Option<u64> {
            root.children().find(|node| node.has_tag_name(tag) && node.attribute("type") == Some(kind))?
                .attribute(attr)?.parse().ok()
        };
        let system = metric("system", "current", "size")?;
        let free_bytes = metric("total", "fast", "size")?.checked_add(metric("total", "rest", "size")?)?;
        Some(Self {
            arenas: root.children().filter(|node| node.has_tag_name("heap")).count(),
            used_bytes: system.checked_sub(free_bytes)?,
            free_bytes,
            mmap_bytes: metric("total", "mmap", "size")?,
        })
    }

    fn sample() -> Option<Self> {
        unsafe extern "C" {
            fn malloc_info(options: libc::c_int, stream: *mut libc::FILE) -> libc::c_int;
        }

        // SAFETY: glibc fills an owned buffer on fclose. We parse it before
        // freeing it, including when the C call or parsing fails.
        unsafe {
            let mut buffer = std::ptr::null_mut();
            let mut len = 0;
            let stream = libc::open_memstream(&mut buffer, &mut len);
            if stream.is_null() {
                return None;
            }
            let status = malloc_info(0, stream);
            let closed = libc::fclose(stream);
            let heap = if status == 0 && closed == 0 && !buffer.is_null() {
                let xml = std::slice::from_raw_parts(buffer.cast(), len);
                std::str::from_utf8(xml).ok().and_then(Self::from_xml)
            } else {
                None
            };
            libc::free(buffer.cast());
            heap
        }
    }
}

#[derive(Debug, Default)]
struct Caches {
    sampled_workspaces: Option<usize>,
    busy_workspace_locks: usize,
    library_text_weight_bytes: usize,
    graph_cards: usize,
    graph_search_field_bytes: usize,
    replay_serialized_bytes: u64,
    loaded_transcripts: usize,
    transcript_source_bytes: usize,
    transcript_entries: usize,
    history_pages: usize,
    timeline_offset_capacity_bytes: usize,
}

impl Caches {
    fn sample(app: &App) -> Self {
        let mut caches = Self::default();
        let Some(workspaces) = app.try_loaded_workspaces() else { return caches };
        caches.sampled_workspaces = Some(workspaces.len());
        for workspace in workspaces {
            if let Ok(index) = workspace.library.try_lock() {
                caches.library_text_weight_bytes += index.text_weight_bytes;
            } else {
                caches.busy_workspace_locks += 1;
            }
            match workspace.graph.try_cache_size() {
                Some((cards, bytes)) => {
                    caches.graph_cards += cards;
                    caches.graph_search_field_bytes += bytes;
                }
                None => caches.busy_workspace_locks += 1,
            }
            match workspace.hub.try_cache_size() {
                Some((bytes, size)) => {
                    caches.replay_serialized_bytes += bytes;
                    caches.loaded_transcripts += size.loaded_transcripts;
                    caches.transcript_source_bytes += size.transcript_source_bytes;
                    caches.transcript_entries += size.transcript_entries;
                    caches.history_pages += size.history_pages;
                    caches.timeline_offset_capacity_bytes += size.timeline_offset_capacity_bytes;
                }
                None => caches.busy_workspace_locks += 1,
            }
        }
        caches
    }
}

fn report_and_trim(app: &App) {
    let start = Instant::now();
    let before = rss_kib();
    // SAFETY: malloc_trim is thread safe.
    unsafe { libc::malloc_trim(0) };
    let trim_us = start.elapsed().as_micros() as u64;
    let after = rss_kib();
    let heap = Heap::sample();
    let caches = Caches::sample(app);
    let elapsed_us = start.elapsed().as_micros() as u64;
    tracing::info!(
        rss_before_trim_kib = ?before,
        rss_after_trim_kib = ?after,
        glibc_arenas = ?heap.as_ref().map(|heap| heap.arenas),
        glibc_arena_used_bytes = ?heap.as_ref().map(|heap| heap.used_bytes),
        glibc_arena_free_bytes = ?heap.as_ref().map(|heap| heap.free_bytes),
        glibc_mmap_bytes = ?heap.as_ref().map(|heap| heap.mmap_bytes),
        trim_us,
        elapsed_us,
        ?caches,
        "server memory"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_process_totals_without_double_counting_arenas() {
        let xml = r#"<malloc version="1">
            <heap nr="0"><system type="current" size="100"/></heap>
            <heap nr="1"><system type="current" size="70"/></heap>
            <total type="fast" size="10"/>
            <total type="rest" size="60"/>
            <total type="mmap" size="30"/>
            <system type="current" size="170"/>
        </malloc>"#;
        assert_eq!(Heap::from_xml(xml), Some(Heap {
            arenas: 2, used_bytes: 100, free_bytes: 70, mmap_bytes: 30,
        }));
        assert!(Heap::from_xml("<malloc/>").is_none());
    }

    #[test]
    fn reads_current_process_memory() {
        assert!(rss_kib().is_some_and(|rss| rss > 0));
        assert!(Heap::sample().is_some_and(|heap| heap.arenas > 0 && heap.used_bytes > 0));
    }
}

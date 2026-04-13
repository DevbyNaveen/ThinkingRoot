use thinkingroot_bench::{Fixture, Scale};
use thinkingroot_core::ContentHash;

// ---------------------------------------------------------------------------
// blake3_hash_kb — raw BLAKE3 throughput for ContentHash::from_bytes
// ---------------------------------------------------------------------------

#[divan::bench(args = [1_usize, 10, 100, 1000])]
fn blake3_hash_kb(bencher: divan::Bencher, size_kb: &usize) {
    let size = *size_kb;
    let data = vec![0x42u8; size * 1024];

    bencher.bench_local(|| ContentHash::from_bytes(&data));
}

// ---------------------------------------------------------------------------
// fingerprint_check_all_cached
// All 100 probes hit source hashes that WERE inserted by Fixture (hash_0..hash_99)
// ---------------------------------------------------------------------------

#[divan::bench(args = [Scale::Small, Scale::Medium, Scale::Large])]
fn fingerprint_check_all_cached(bencher: divan::Bencher, scale: &Scale) {
    let scale = *scale;
    let fix = Fixture::generate(scale);

    bencher.bench_local(|| {
        for i in 0..100_usize {
            let _ = fix
                .graph
                .source_hash_exists(&format!("hash_{i}"))
                .expect("source_hash_exists failed");
        }
    });
}

// ---------------------------------------------------------------------------
// fingerprint_check_none_cached
// All 100 probes use hashes that were NEVER inserted — guaranteed misses
// ---------------------------------------------------------------------------

#[divan::bench(args = [Scale::Small, Scale::Medium, Scale::Large])]
fn fingerprint_check_none_cached(bencher: divan::Bencher, scale: &Scale) {
    let scale = *scale;
    let fix = Fixture::generate(scale);

    bencher.bench_local(|| {
        for i in 0..100_usize {
            let _ = fix
                .graph
                .source_hash_exists(&format!("nonexistent_hash_{i}"))
                .expect("source_hash_exists failed");
        }
    });
}

// ---------------------------------------------------------------------------
// fingerprint_check_mixed_80_20
// 80 % hit existing hashes, 20 % use novel keys — simulates incremental compile
// ---------------------------------------------------------------------------

#[divan::bench(args = [Scale::Small, Scale::Medium, Scale::Large])]
fn fingerprint_check_mixed_80_20(bencher: divan::Bencher, scale: &Scale) {
    let scale = *scale;
    let fix = Fixture::generate(scale);

    bencher.bench_local(|| {
        for i in 0..100_usize {
            let key = if i % 5 == 0 {
                // 20 % novel misses
                format!("novel_hash_{i}")
            } else {
                // 80 % hits — cycle through the first 50 source hashes
                format!("hash_{}", i % 50)
            };
            let _ = fix
                .graph
                .source_hash_exists(&key)
                .expect("source_hash_exists failed");
        }
    });
}

fn main() {
    divan::main();
}

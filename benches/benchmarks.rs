// Benchmarks for VelociDB

use criterion::{black_box, criterion_group, criterion_main, Criterion, BenchmarkId};
use tempfile::NamedTempFile;
use velocidb::Database;

fn benchmark_insert(c: &mut Criterion) {
    let mut group = c.benchmark_group("insert");
    
    for size in [10, 100, 1000].iter() {
        group.bench_with_input(BenchmarkId::from_parameter(size), size, |b, &size| {
            b.iter(|| {
                let temp_file = NamedTempFile::new().unwrap();
                let db = Database::open(temp_file.path()).unwrap();
                
                db.execute("CREATE TABLE test (id INTEGER PRIMARY KEY, value INTEGER)").unwrap();
                
                for i in 0..size {
                    db.execute(&format!(
                        "INSERT INTO test (id, value) VALUES ({}, {})",
                        black_box(i),
                        black_box(i * 2)
                    )).unwrap();
                }
            });
        });
    }
    
    group.finish();
}

fn benchmark_select(c: &mut Criterion) {
    let mut group = c.benchmark_group("select");
    
    for size in [10, 100, 1000].iter() {
        group.bench_with_input(BenchmarkId::from_parameter(size), size, |b, &size| {
            // Setup
            let temp_file = NamedTempFile::new().unwrap();
            let db = Database::open(temp_file.path()).unwrap();
            
            db.execute("CREATE TABLE test (id INTEGER PRIMARY KEY, value INTEGER)").unwrap();
            
            for i in 0..size {
                db.execute(&format!(
                    "INSERT INTO test (id, value) VALUES ({}, {})",
                    i,
                    i * 2
                )).unwrap();
            }
            
            // Benchmark
            b.iter(|| {
                let result = db.query("SELECT * FROM test").unwrap();
                black_box(result);
            });
        });
    }
    
    group.finish();
}

fn benchmark_select_with_where(c: &mut Criterion) {
    let mut group = c.benchmark_group("select_with_where");
    
    for size in [10, 100, 1000].iter() {
        group.bench_with_input(BenchmarkId::from_parameter(size), size, |b, &size| {
            // Setup
            let temp_file = NamedTempFile::new().unwrap();
            let db = Database::open(temp_file.path()).unwrap();
            
            db.execute("CREATE TABLE test (id INTEGER PRIMARY KEY, value INTEGER)").unwrap();
            
            for i in 0..size {
                db.execute(&format!(
                    "INSERT INTO test (id, value) VALUES ({}, {})",
                    i,
                    i * 2
                )).unwrap();
            }
            
            // Benchmark
            b.iter(|| {
                let result = db.query(&format!(
                    "SELECT * FROM test WHERE value > {}",
                    black_box(size / 2)
                )).unwrap();
                black_box(result);
            });
        });
    }
    
    group.finish();
}

fn benchmark_update(c: &mut Criterion) {
    let mut group = c.benchmark_group("update");
    
    for size in [10, 100, 1000].iter() {
        group.bench_with_input(BenchmarkId::from_parameter(size), size, |b, &size| {
            b.iter_batched(
                || {
                    // Setup
                    let temp_file = NamedTempFile::new().unwrap();
                    let db = Database::open(temp_file.path()).unwrap();
                    
                    db.execute("CREATE TABLE test (id INTEGER PRIMARY KEY, value INTEGER)").unwrap();
                    
                    for i in 0..size {
                        db.execute(&format!(
                            "INSERT INTO test (id, value) VALUES ({}, {})",
                            i,
                            i * 2
                        )).unwrap();
                    }
                    
                    (temp_file, db)
                },
                |(_temp_file, db)| {
                    // Benchmark
                    db.execute(&format!(
                        "UPDATE test SET value = {} WHERE id = {}",
                        black_box(999),
                        black_box(size / 2)
                    )).unwrap();
                },
                criterion::BatchSize::SmallInput,
            );
        });
    }
    
    group.finish();
}

fn benchmark_delete(c: &mut Criterion) {
    let mut group = c.benchmark_group("delete");
    
    for size in [10, 100, 1000].iter() {
        group.bench_with_input(BenchmarkId::from_parameter(size), size, |b, &size| {
            b.iter_batched(
                || {
                    // Setup
                    let temp_file = NamedTempFile::new().unwrap();
                    let db = Database::open(temp_file.path()).unwrap();
                    
                    db.execute("CREATE TABLE test (id INTEGER PRIMARY KEY, value INTEGER)").unwrap();
                    
                    for i in 0..size {
                        db.execute(&format!(
                            "INSERT INTO test (id, value) VALUES ({}, {})",
                            i,
                            i * 2
                        )).unwrap();
                    }
                    
                    (temp_file, db)
                },
                |(_temp_file, db)| {
                    // Benchmark
                    db.execute(&format!(
                        "DELETE FROM test WHERE id = {}",
                        black_box(size / 2)
                    )).unwrap();
                },
                criterion::BatchSize::SmallInput,
            );
        });
    }
    
    group.finish();
}

fn benchmark_mixed_workload(c: &mut Criterion) {
    c.bench_function("mixed_workload", |b| {
        b.iter(|| {
            let temp_file = NamedTempFile::new().unwrap();
            let db = Database::open(temp_file.path()).unwrap();
            
            db.execute("CREATE TABLE test (id INTEGER PRIMARY KEY, value INTEGER)").unwrap();
            
            // Insert 100 rows
            for i in 0..100 {
                db.execute(&format!(
                    "INSERT INTO test (id, value) VALUES ({}, {})",
                    i, i * 2
                )).unwrap();
            }
            
            // Read 10 times
            for _ in 0..10 {
                let _ = db.query("SELECT * FROM test WHERE value > 50").unwrap();
            }
            
            // Update 10 rows
            for i in 0..10 {
                db.execute(&format!(
                    "UPDATE test SET value = {} WHERE id = {}",
                    i * 3, i
                )).unwrap();
            }
            
            // Delete 10 rows
            for i in 90..100 {
                db.execute(&format!("DELETE FROM test WHERE id = {}", i)).unwrap();
            }
            
            black_box(db);
        });
    });
}

criterion_group!(
    benches,
    benchmark_insert,
    benchmark_select,
    benchmark_select_with_where,
    benchmark_update,
    benchmark_delete,
    benchmark_mixed_workload
);
criterion_main!(benches);

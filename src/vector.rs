//! Vector search support (Turso / libSQL inspired).
//!
//! Provides:
//! - Parsing of vector literals (`[1.0, 2.0, 3.0]`) and SQL constructors
//!   (`vector32('[...]')`, `vector('[...]')`).
//! - Distance metrics: cosine, euclidean (L2) and negative dot product.
//! - Parsing of distance expressions such as
//!   `vector_distance_cos(embedding, vector32('[1,2,3]'))` used in SELECT
//!   projections and `ORDER BY` clauses.
//! - Exact (brute-force) K-nearest-neighbour search, parallelized with rayon.

use crate::types::{Result, Row, Value, VelociError};
use rayon::prelude::*;
use std::cmp::Ordering;

/// Row-count threshold above which distance computation switches to rayon.
const PARALLEL_THRESHOLD: usize = 1024;

/// Supported vector distance metrics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DistanceMetric {
    /// Cosine distance: `1 - cos(a, b)`. Lower is more similar.
    Cosine,
    /// Euclidean (L2) distance. Lower is more similar.
    Euclidean,
    /// Negative dot product, so that lower is more similar (consistent with
    /// the other metrics when used in ascending ORDER BY).
    Dot,
}

impl DistanceMetric {
    /// Maps a SQL function name to a metric.
    pub fn from_function_name(name: &str) -> Option<Self> {
        match name.to_lowercase().as_str() {
            "vector_distance_cos" => Some(DistanceMetric::Cosine),
            "vector_distance_l2" | "vector_distance_euclidean" => Some(DistanceMetric::Euclidean),
            "vector_distance_dot" => Some(DistanceMetric::Dot),
            _ => None,
        }
    }

    /// Computes the distance between two vectors.
    pub fn distance(&self, a: &[f32], b: &[f32]) -> Result<f64> {
        if a.len() != b.len() {
            return Err(VelociError::TypeMismatch {
                expected: format!("vector of dimension {}", a.len()),
                actual: format!("vector of dimension {}", b.len()),
            });
        }
        Ok(match self {
            DistanceMetric::Cosine => cosine_distance(a, b),
            DistanceMetric::Euclidean => euclidean_distance(a, b),
            DistanceMetric::Dot => -dot_product(a, b),
        })
    }
}

/// Dot product of two equal-length vectors.
#[inline]
pub fn dot_product(a: &[f32], b: &[f32]) -> f64 {
    a.iter()
        .zip(b.iter())
        .map(|(x, y)| (*x as f64) * (*y as f64))
        .sum()
}

/// Cosine distance `1 - (a·b) / (|a||b|)`. Returns 1.0 for zero vectors.
#[inline]
pub fn cosine_distance(a: &[f32], b: &[f32]) -> f64 {
    let mut dot = 0.0f64;
    let mut norm_a = 0.0f64;
    let mut norm_b = 0.0f64;
    for (x, y) in a.iter().zip(b.iter()) {
        let (x, y) = (*x as f64, *y as f64);
        dot += x * y;
        norm_a += x * x;
        norm_b += y * y;
    }
    let denom = norm_a.sqrt() * norm_b.sqrt();
    if denom == 0.0 {
        return 1.0;
    }
    1.0 - dot / denom
}

/// Euclidean (L2) distance.
#[inline]
pub fn euclidean_distance(a: &[f32], b: &[f32]) -> f64 {
    a.iter()
        .zip(b.iter())
        .map(|(x, y)| {
            let d = (*x as f64) - (*y as f64);
            d * d
        })
        .sum::<f64>()
        .sqrt()
}

/// Parses a JSON-style vector literal: `[1.0, 2.0, 3.0]`.
pub fn parse_vector_literal(s: &str) -> Result<Vec<f32>> {
    let s = s.trim();
    let inner = s
        .strip_prefix('[')
        .and_then(|r| r.strip_suffix(']'))
        .ok_or_else(|| {
            VelociError::ParseError(format!(
                "Invalid vector literal '{}': expected '[x, y, ...]'",
                s
            ))
        })?;

    let inner = inner.trim();
    if inner.is_empty() {
        return Ok(Vec::new());
    }

    inner
        .split(',')
        .map(|part| {
            part.trim().parse::<f32>().map_err(|_| {
                VelociError::ParseError(format!(
                    "Invalid vector component '{}' in literal '{}'",
                    part.trim(),
                    s
                ))
            })
        })
        .collect()
}

/// Parses a SQL vector constructor: `vector32('[...]')`, `vector('[...]')`
/// or a bare `[...]` literal. Returns `None` if `s` is not vector-shaped
/// (the caller should then try other value types).
pub fn parse_vector_constructor(s: &str) -> Option<Result<Vec<f32>>> {
    let s = s.trim();

    if s.starts_with('[') {
        return Some(parse_vector_literal(s));
    }

    // Quoted literal: '[1, 2, 3]'
    if let Some(unquoted) = s
        .strip_prefix('\'')
        .and_then(|r| r.strip_suffix('\''))
        .or_else(|| s.strip_prefix('"').and_then(|r| r.strip_suffix('"')))
    {
        if unquoted.trim_start().starts_with('[') {
            return Some(parse_vector_literal(unquoted));
        }
    }

    let lower = s.to_lowercase();
    for prefix in ["vector32(", "vector64(", "vector("] {
        if lower.starts_with(prefix) && s.ends_with(')') {
            let inner = s[prefix.len()..s.len() - 1].trim();
            // Strip surrounding quotes from the literal argument.
            let unquoted = inner
                .strip_prefix('\'')
                .and_then(|r| r.strip_suffix('\''))
                .or_else(|| inner.strip_prefix('"').and_then(|r| r.strip_suffix('"')))
                .unwrap_or(inner);
            return Some(parse_vector_literal(unquoted));
        }
    }

    None
}

/// A parsed vector distance expression, e.g.
/// `vector_distance_cos(embedding, vector32('[1,2,3]'))`.
#[derive(Debug, Clone, PartialEq)]
pub struct DistanceExpr {
    pub metric: DistanceMetric,
    pub column: String,
    pub query: Vec<f32>,
}

/// Parses a distance expression of the form
/// `vector_distance_*(column, <vector constructor>)`.
/// Returns `None` if `s` does not look like a distance function call.
pub fn parse_distance_expr(s: &str) -> Option<Result<DistanceExpr>> {
    let s = s.trim();
    let open = s.find('(')?;
    let func_name = s[..open].trim();
    let metric = DistanceMetric::from_function_name(func_name)?;

    if !s.ends_with(')') {
        return Some(Err(VelociError::ParseError(format!(
            "Malformed distance expression: {}",
            s
        ))));
    }
    let args = &s[open + 1..s.len() - 1];

    // Split on the first top-level comma (outside quotes/parens).
    let mut depth = 0usize;
    let mut in_string = false;
    let mut quote = '\'';
    let mut split_at = None;
    for (i, ch) in args.char_indices() {
        if in_string {
            if ch == quote {
                in_string = false;
            }
        } else {
            match ch {
                '\'' | '"' => {
                    in_string = true;
                    quote = ch;
                }
                '(' | '[' => depth += 1,
                ')' | ']' => depth = depth.saturating_sub(1),
                ',' if depth == 0 => {
                    split_at = Some(i);
                    break;
                }
                _ => {}
            }
        }
    }

    let split_at = match split_at {
        Some(i) => i,
        None => {
            return Some(Err(VelociError::ParseError(format!(
                "Distance function expects two arguments: {}",
                s
            ))))
        }
    };

    let column = args[..split_at].trim().to_string();
    let vec_arg = args[split_at + 1..].trim();

    let query = match parse_vector_constructor(vec_arg) {
        Some(Ok(v)) => v,
        Some(Err(e)) => return Some(Err(e)),
        None => {
            return Some(Err(VelociError::ParseError(format!(
                "Second argument of a distance function must be a vector literal: {}",
                vec_arg
            ))))
        }
    };

    Some(Ok(DistanceExpr {
        metric,
        column,
        query,
    }))
}

/// Extracts the vector stored in `value` (either a native `Value::Vector` or
/// a raw f32 little-endian `Value::Blob`).
pub fn value_as_vector(value: &Value) -> Result<Vec<f32>> {
    match value {
        Value::Vector(v) => Ok(v.clone()),
        Value::Blob(b) if b.len() % 4 == 0 => Ok(b
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect()),
        other => Err(VelociError::TypeMismatch {
            expected: "Vector".to_string(),
            actual: format!("{:?}", other),
        }),
    }
}

/// Computes the distance from every row's vector column to `query`.
///
/// Rows whose vector is NULL or has a mismatched dimension get distance
/// `f64::INFINITY` so they sort last. Parallelized with rayon for larger
/// candidate sets.
pub fn compute_distances(
    rows: &[(i64, Row)],
    col_index: usize,
    query: &[f32],
    metric: DistanceMetric,
) -> Vec<f64> {
    let distance_of = |row: &Row| -> f64 {
        let value = match row.values.get(col_index) {
            Some(v) => v,
            None => return f64::INFINITY,
        };
        match value_as_vector(value) {
            Ok(v) => metric.distance(&v, query).unwrap_or(f64::INFINITY),
            Err(_) => f64::INFINITY,
        }
    };

    if rows.len() >= PARALLEL_THRESHOLD {
        rows.par_iter().map(|(_, row)| distance_of(row)).collect()
    } else {
        rows.iter().map(|(_, row)| distance_of(row)).collect()
    }
}

/// Exact K-nearest-neighbour search over a set of candidate rows.
///
/// Returns up to `k` `(distance, key, row)` triples sorted by ascending
/// distance. Distance computation runs in parallel for large candidate sets.
pub fn knn(
    rows: Vec<(i64, Row)>,
    col_index: usize,
    query: &[f32],
    metric: DistanceMetric,
    k: usize,
) -> Vec<(f64, i64, Row)> {
    if k == 0 {
        return Vec::new();
    }

    let distances = compute_distances(&rows, col_index, query, metric);
    let mut scored: Vec<(f64, i64, Row)> = rows
        .into_iter()
        .zip(distances)
        .map(|((key, row), d)| (d, key, row))
        .collect();

    let compare = |a: &(f64, i64, Row), b: &(f64, i64, Row)| {
        a.0.partial_cmp(&b.0)
            .unwrap_or(Ordering::Equal)
            .then(a.1.cmp(&b.1))
    };

    if scored.len() > k {
        scored.select_nth_unstable_by(k - 1, compare);
        scored.truncate(k);
    }
    scored.sort_by(compare);
    scored
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_vector_literal() {
        assert_eq!(
            parse_vector_literal("[1.0, 2.5, -3]").unwrap(),
            vec![1.0, 2.5, -3.0]
        );
        assert_eq!(parse_vector_literal("[]").unwrap(), Vec::<f32>::new());
        assert!(parse_vector_literal("1, 2").is_err());
        assert!(parse_vector_literal("[a, b]").is_err());
    }

    #[test]
    fn test_parse_vector_constructor() {
        assert_eq!(
            parse_vector_constructor("vector32('[1, 2]')")
                .unwrap()
                .unwrap(),
            vec![1.0, 2.0]
        );
        assert_eq!(
            parse_vector_constructor("VECTOR('[0.5]')")
                .unwrap()
                .unwrap(),
            vec![0.5]
        );
        assert_eq!(
            parse_vector_constructor("[3, 4]").unwrap().unwrap(),
            vec![3.0, 4.0]
        );
        assert!(parse_vector_constructor("'hello'").is_none());
        assert!(parse_vector_constructor("42").is_none());
    }

    #[test]
    fn test_distances() {
        let a = [1.0f32, 0.0];
        let b = [0.0f32, 1.0];
        assert!((cosine_distance(&a, &b) - 1.0).abs() < 1e-9);
        assert!((cosine_distance(&a, &a)).abs() < 1e-9);
        assert!((euclidean_distance(&a, &b) - std::f64::consts::SQRT_2).abs() < 1e-9);
        assert!((dot_product(&a, &b)).abs() < 1e-9);
    }

    #[test]
    fn test_parse_distance_expr() {
        let expr = parse_distance_expr("vector_distance_cos(embedding, vector32('[1, 2, 3]'))")
            .unwrap()
            .unwrap();
        assert_eq!(expr.metric, DistanceMetric::Cosine);
        assert_eq!(expr.column, "embedding");
        assert_eq!(expr.query, vec![1.0, 2.0, 3.0]);

        let expr = parse_distance_expr("vector_distance_l2(v, '[0.5]')")
            .unwrap()
            .unwrap();
        assert_eq!(expr.metric, DistanceMetric::Euclidean);
        assert_eq!(expr.query, vec![0.5]);

        assert!(parse_distance_expr("count(*)").is_none());
        assert!(parse_distance_expr("name").is_none());
    }

    #[test]
    fn test_knn() {
        let rows: Vec<(i64, Row)> = (0..10)
            .map(|i| {
                (
                    i,
                    Row::new(vec![Value::Integer(i), Value::Vector(vec![i as f32, 0.0])]),
                )
            })
            .collect();

        let result = knn(rows, 1, &[3.0, 0.0], DistanceMetric::Euclidean, 3);
        assert_eq!(result.len(), 3);
        assert_eq!(result[0].1, 3); // exact match first
        assert!(result[0].0 <= result[1].0);
    }
}

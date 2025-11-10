# Contributing to VelociDB

Thank you for your interest in contributing to VelociDB! This document provides guidelines and instructions for contributing.

## Code of Conduct

Be respectful, constructive, and professional in all interactions.

## Development Setup

### Prerequisites
- Rust 1.70 or higher
- Cargo
- Git

### Clone the Repository
```bash
git clone https://github.com/niklabh/velocidb.git
cd velocidb
```

### Build
```bash
cargo build
```

### Run Tests
```bash
cargo test
```

### Run Benchmarks
```bash
cargo bench
```

## Project Structure

```
velocidb/
├── src/
│   ├── main.rs          # Entry point
│   ├── lib.rs           # Library interface
│   ├── storage.rs       # Storage layer
│   ├── btree.rs         # B-Tree implementation
│   ├── parser.rs        # SQL parser
│   ├── executor.rs      # Query executor
│   ├── transaction.rs   # Transaction management
│   └── types.rs         # Core types
├── tests/               # Integration tests
├── benches/             # Benchmarks
└── docs/                # Documentation
```

## Contributing Guidelines

### Reporting Issues

When reporting bugs, please include:
1. Rust version (`rustc --version`)
2. Operating system
3. Minimal reproducible example
4. Expected vs actual behavior

### Feature Requests

For feature requests:
1. Check existing issues first
2. Describe the use case
3. Explain why it's beneficial
4. Propose implementation approach (optional)

### Pull Requests

1. **Fork the repository**
2. **Create a feature branch**
   ```bash
   git checkout -b feature/your-feature-name
   ```

3. **Make your changes**
   - Write clear, self-documenting code
   - Follow Rust naming conventions
   - Add tests for new functionality
   - Update documentation as needed

4. **Test your changes**
   ```bash
   cargo test
   cargo clippy
   cargo fmt --check
   ```

5. **Commit your changes**
   ```bash
   git commit -m "Add feature: description"
   ```
   - Use clear, descriptive commit messages
   - Reference issue numbers if applicable

6. **Push to your fork**
   ```bash
   git push origin feature/your-feature-name
   ```

7. **Create a Pull Request**
   - Describe your changes
   - Link related issues
   - Include test results

## Code Style

### Formatting
- Use `rustfmt` for consistent formatting
- Run `cargo fmt` before committing

### Linting
- Use `clippy` to catch common mistakes
- Run `cargo clippy` and fix warnings

### Naming Conventions
- `snake_case` for variables and functions
- `PascalCase` for types and traits
- `SCREAMING_SNAKE_CASE` for constants

### Documentation
- Document public APIs with `///` doc comments
- Include examples in documentation
- Keep documentation up-to-date

## Testing

### Unit Tests
```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_feature() {
        // Test implementation
    }
}
```

### Integration Tests
Place integration tests in `tests/` directory.

### Benchmarks
Add benchmarks to `benches/` using Criterion.

## Areas for Contribution

### High Priority
- [ ] B-Tree node splitting
- [ ] Secondary indexes
- [ ] Query optimizer
- [ ] JOIN operations
- [ ] Aggregation functions

### Medium Priority
- [ ] ORDER BY and LIMIT
- [ ] Subqueries
- [ ] Views
- [ ] Performance improvements

### Low Priority
- [ ] Network protocol
- [ ] Replication
- [ ] Backup/restore utilities
- [ ] Additional SQL features

## Performance Guidelines

1. **Profile before optimizing**
   ```bash
   cargo build --release
   perf record -g target/release/velocidb
   ```

2. **Benchmark your changes**
   ```bash
   cargo bench --bench benchmarks
   ```

3. **Avoid premature optimization**
   - Focus on correctness first
   - Optimize hot paths based on profiling

4. **Memory efficiency**
   - Minimize allocations in hot paths
   - Use iterators instead of collecting
   - Consider arena allocation for temporary data

## Review Process

1. All PRs require review
2. Address reviewer feedback
3. Ensure CI passes
4. Maintain test coverage
5. Keep commits clean and logical

## Questions?

Feel free to open an issue for questions or clarifications.

## License

By contributing, you agree that your contributions will be licensed under the MIT License.


# Security Policy

## Supported Versions

| Version | Supported          |
| ------- | ------------------ |
| 0.1.x   | :white_check_mark: |

## Reporting a Vulnerability

If you discover a security vulnerability in VelociDB, please **do not** open a public issue.

Instead, email the details to **niklabh811@gmail.com**. Include:

- A description of the vulnerability.
- Steps to reproduce it.
- The affected version(s).
- Any suggested fix (if available).

You can expect an initial response within 48 hours. We will work with you to
understand the scope, develop a fix, and coordinate a release.

## Security Best Practices

- **Sandboxing**: VelociDB is an embedded library that runs in-process.
  If you accept SQL from untrusted users, run the database in a sandboxed
  environment with restricted filesystem and network access.
- **File permissions**: Store database files with restrictive permissions
  (e.g. `0600`) to prevent unauthorised access by other processes.
- **Input validation**: Always validate and sanitise user-supplied SQL before
  passing it to VelociDB. The parser rejects malformed input, but defence in
  depth is recommended.
- **Dependency auditing**: Run `cargo audit` regularly to check for known
  vulnerabilities in transitive dependencies.

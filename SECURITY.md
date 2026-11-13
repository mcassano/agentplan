# Security policy

agentplan is a safety tool, so we treat bugs that let a sandboxed command
escape its plan as security issues. Examples:

- writing to the real project directory during `agentplan run`
- sending network traffic that does not appear in the plan, on a platform
  where agentplan claims to enforce capture
- `agentplan apply` writing files that were not shown in the plan, or writing
  outside the project root
- leaking secrets (such as `Authorization` headers) into stored network logs

Before reporting, please read [docs/security-model.md](docs/security-model.md).
It lists what agentplan does and does not contain.

## Reporting a vulnerability

Please **do not open a public issue**. Report it privately through
[GitHub security advisories](https://github.com/mcassano/agentplan/security/advisories/new).

Include the agentplan version, your OS and filesystem, and steps to reproduce.
You should get a response within 7 days. Once a fix ships, we will credit you
in the release notes unless you would rather stay anonymous.

## Supported versions

Until 1.0, only the latest release gets security fixes.

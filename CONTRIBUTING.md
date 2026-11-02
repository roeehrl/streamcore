# Contributing

Thank you for your interest in StreamCore.

## Before you open a pull request

StreamCore is licensed under the AGPL-3.0, and Codonic also ships it inside its
own apps under other terms. To keep that possible, every contributor will need
to sign a Contributor License Agreement before a contribution can be merged.

**We are not accepting outside pull requests yet.** The agreement and its
signing process are not published yet. Until they are, please open an issue to
discuss bugs and ideas.

## Ground rules

- Discuss larger changes in an issue first.
- Keep changes focused, with tests.
- Every change to the input model or the wire protocol needs an entry in the
  relevant `CHANGES.md`, naming the commit, and saying whether apps must act
  when they update.
- Never commit secrets, private hostnames, IP addresses or personal data. The
  scrub check (`scripts/scrub-check.sh`) runs on every change and fails on them.

## Reporting security problems

See [SECURITY.md](SECURITY.md). Please don't open public issues for security
problems.

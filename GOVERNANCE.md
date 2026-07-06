# Governance

Tarit is a young, maintainer-led project. This document describes how
decisions are made today. It is intentionally lightweight and will evolve as
the contributor base grows.

## Roles

**Users** run Tarit, report bugs, and request features through issues.

**Contributors** send pull requests. Anyone can contribute; see
[CONTRIBUTING.md](CONTRIBUTING.md) for conventions and the license terms that
apply to contributions.

**Maintainers** review and merge changes, triage issues, handle security
reports, and cut releases. The current maintainers are listed in
[MAINTAINERS.md](MAINTAINERS.md).

## How decisions are made

Day-to-day decisions (bug fixes, small features, refactors) happen in issues
and pull requests. Maintainers apply lazy consensus: a change is accepted when
a maintainer approves it and no maintainer objects.

Larger decisions (protocol changes in `proto/`, new trust-boundary surface in
the VMM, breaking API changes in the orchestrator, licensing) should start as
an issue describing the problem and the proposed approach before code is
written.

While the project has a single maintainer, that maintainer (the project
founder) has the final say, including on disputes. This is the BDFL model. It
is a practical choice for a small project, not a permanent one; as more
maintainers join, contested decisions will move to a majority vote of the
maintainers, with the founder breaking ties.

## Becoming a maintainer

Maintainers are added by invitation from the existing maintainers, based on a
track record of sustained, high-quality contributions: code, review, triage,
or documentation in a specific area (`vmm/`, `orch/`, or `proto/`). If you
are interested, say so in an issue or a pull request; there is no formal
application.

New maintainers are added to [MAINTAINERS.md](MAINTAINERS.md) and
[.github/CODEOWNERS](.github/CODEOWNERS). A maintainer who becomes inactive
for an extended period may be moved to emeritus status after a heads-up.

## Changing this document

Changes to governance are proposed as pull requests against this file and
follow the same decision process described above.

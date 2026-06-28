# Upstream Snapshot

Source repository:

- <https://github.com/idootop/open-xiaoai>

Copied commit:

- `14e1034ba75698be7d3f331b9822b7043deaf032`

Copied or adapted paths in this repository:

- `LICENSE`
- `README.md` as `OPEN_XIAOAI_README.md`
- `agreement.md`
- `docs/flash.md`
- `docs/images/`
- `packages/client-patch/` as `deploy/client-patch/`
- `packages/flash-tool/` as `deploy/flash-tool/`

This is a partial vendored snapshot for local DODO/XiaoAI integration work. It
keeps the firmware patch and flash tooling required to prepare a speaker for
the standalone `xiaoai-agent`; the Open-XiaoAI client runtime itself is not
vendored here.

Generated firmware artifacts, unpacked root filesystems, and local `.env`
credentials are intentionally excluded. Re-sync by copying the same paths from
a newer upstream checkout and updating this file.

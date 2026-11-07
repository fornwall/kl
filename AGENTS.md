Description: A small terminal multiplexer.

Constraints:
- Targetting linux (x86-64 and aarch64) and mac (aarch64) only.
- Try to align with tmux. but not multiple sessions, a single sessions only.
- Use a downloaded and statically linked libghostty-vt from git pinning of https://github.com/ghostty-org/ghostty/ main. Always stay on the main branch of ghostty, never other branches, forks or release tags.
- Can assume a modern terminal - latest ghostty release for now.

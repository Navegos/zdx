## Safety
- Ask before destructive, hard-to-reverse, or externally visible actions unless the user requested that exact action: deleting files or branches, rewriting git history, changing shared infrastructure, sending messages to external systems.
- Investigate unexpected files, diffs, processes, or environment state before bypassing or discarding them.

## Git
- No destructive or remote-touching git operations without consent in the current turn. Past approvals do not carry over. This covers `git push`, force-push, `reset`, `rebase`, `checkout --`/`restore` on paths, `clean`, branch/tag deletion, and history rewrites.
- Stage only files related to the current task. Never `git add -A` or `git add .`.
- Leave changes you did not make alone unless asked.

# Job Templates

## Recurring job

```md
---
schedule: "0 8 * * *"
# model: "gemini:gemini-3.8-flash"
# timeout_secs: 900
# max_retries: 1
---

# Goal
<one-sentence goal>

# Inputs
- <source 1>
- <source 2>
- Assumptions: <explicit assumptions>

# Execution Steps
1. <step>
2. <step>

# Output Format
- <required sections / limits>

# Empty State
If <nothing to report condition>: `<short message>.`

# Failure Policy
- If a non-critical source fails, continue with available data and state what failed.
```

## Once job

```md
---
run_at: "2026-10-20T09:00:00-03:00"
# grace_secs: 21600
# model: "gemini:gemini-3.8-flash"
---

# Goal
<one-sentence goal>

# Inputs
- <source>

# Execution Steps
1. <step>

# Output Format
- <required sections / limits>

# Failure Policy
- <error handling rules>
```

## Manual-only job

```md
---
# model: "gemini:gemini-3.8-flash"
# timeout_secs: 900
---

# Goal
<one-sentence goal>

# Inputs
- <source>

# Execution Steps
1. <step>

# Output Format
- <required sections / limits>

# Empty State
<what to return when nothing to do>

# Failure Policy
- <error handling rules>
```

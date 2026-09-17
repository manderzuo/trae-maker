---
name: aiwork-seedance
description: Use when a user asks to create a Seedance video through AI Work, including text-to-video, image-to-video, reference assets, task polling, or retrieving a generated MP4.
---

# AI Work Seedance

This skill connects an AI client to the user's AI Work Assistant gateway. The
gateway owns Trae Work accounts, Work credits, task state, account failover,
and video storage. The client only supplies the prompt and explicitly selected
reference files.

## Required behavior

1. Run `doctor` before the first generation or after a connection error. Do not
   guess a gateway URL or API key.
2. For a local reference image/video, pass its explicit path as `image_paths`
   or `video_paths`. Never scan a directory or send a local path to Seedance.
3. Submit once and keep the returned `task_id`. Use status/wait for pending
   tasks; never resubmit merely because a poll is delayed.
4. Download only after `completed`, using a temporary `.part` file and the
   user's requested destination. Report the task ID, status, URL (when safe),
   and local file path.
5. Do not print or place `AIWORK_API_KEY`, JWTs, cookies, or authorization
   headers in prompts, logs, or generated files.

## Commands

Run the bundled PowerShell runner from this skill directory:

```powershell
./scripts/aiwork-seedance.ps1 doctor
./scripts/aiwork-seedance.ps1 submit -Prompt "..." -ImagePath "C:\path\first-frame.png"
./scripts/aiwork-seedance.ps1 status -TaskId "video-..."
./scripts/aiwork-seedance.ps1 wait -TaskId "video-..."
./scripts/aiwork-seedance.ps1 download -TaskId "video-..." -OutputPath "C:\path\result.mp4"
```

The logical operations are `seedance_submit`, `seedance_status`, and
`seedance_download`; the bundled runner exposes them as `submit`, `status`,
and `download`. `submit` returns immediately with a task ID. Use `wait` or repeated `status`
calls for long jobs. Defaults are 5 seconds, 720p, and 16:9; pass `-Duration`,
`-Resolution`, or `-Ratio` only when the user requests a different value.

The first-time setup is `install.cmd` (or `scripts/install.ps1`). It installs
this folder into the standard Agent Skills location and stores the gateway
configuration in the user's private application data. DSH and Qoder clients
that do not scan `.agents/skills` can use the same runner through their shell
or MCP adapter; the HTTP contract remains unchanged.

If the gateway is unreachable, credentials are missing, an asset is not
publicly reachable by Trae, or a task fails, report the exact actionable error
and stop. Never fabricate a video URL or silently retry a charged submission.

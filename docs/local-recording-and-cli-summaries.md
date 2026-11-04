# Local recordings folder, file naming, and CLI-based summaries

This fork adds three things to the Tauri desktop app:

1. The **save location** you pick in *Settings → Recording* is actually used when a meeting is recorded or imported (previously it was stored but ignored).
2. The **meeting folder name and audio file name** are generated from a template you control.
3. A **Local CLI Agent** summary provider that drives a coding-agent CLI you already have installed and logged in (Claude Code, OpenAI Codex, Gemini CLI, or any custom command). Your subscription is used; Meetily stores no API key.

## 1. Save location

*Settings → Recording → Save Location*

| Action | What happens |
| --- | --- |
| **Choose Folder…** | Opens the native folder picker. The folder is created if missing and checked for write access before it is saved. |
| **Open Folder** | Opens the current save folder in Finder / Explorer / the file manager. |
| **Reset to default** | Goes back to the platform default (`~/Movies/meetily-recordings` on macOS, `%USERPROFILE%\Music\meetily-recordings` on Windows, `~/Documents/meetily-recordings` on Linux). |

The save location cannot be changed while a recording is running. Transcripts (`transcripts.json`) and `metadata.json` are always written to the meeting folder, even when *Save Audio Recordings* is off.

Imported audio files land in the same folder, using the same naming template.

## 2. File naming

*Settings → Recording → File Naming*

Each meeting gets its own folder inside the save location:

```
<save folder>/
└── <folder name template>/
    ├── <audio file name>.mp4
    ├── transcripts.json
    └── metadata.json
```

Defaults are `{meeting}_{date}_{time}` for the folder and `audio` for the file, which produces the same layout as before (`Team Standup_2025-11-04_14-30/audio.mp4`). Keeping the audio name `audio` is recommended; retranscription and import look for that name first, and fall back to any audio file in the folder otherwise.

Placeholders (local time):

| Placeholder | Value |
| --- | --- |
| `{meeting}` | meeting title |
| `{date}` | `YYYY-MM-DD` |
| `{time}` | `HH-MM` |
| `{datetime}` | `YYYY-MM-DD_HH-MM-SS` |
| `{year}` `{month}` `{day}` | `YYYY` `MM` `DD` |
| `{hour}` `{minute}` `{second}` | `HH` `MM` `SS` |

Characters that are invalid in file names (`/ \ : * ? " < > |`) become `_`. If the rendered folder already exists, ` (2)`, ` (3)`… is appended so two meetings never share a folder. A template that renders to nothing falls back to the default.

The settings page shows a live example of both names while you type. Changes are saved when the field loses focus or you press Enter.

## 3. Local CLI Agent summaries

*Settings → Summary Model → Summarization Model → "Local CLI Agent (Claude Code / Codex / Gemini)"*

### How it works

For every LLM call the summary pipeline makes, Meetily spawns the configured CLI as a child process, pipes the full prompt (system + user) to its stdin, waits for it to exit, and uses its stdout (or the file named by `{output_file}`) as the completion. The CLI handles authentication itself, so whatever login you have in your terminal (`claude`, `codex login`, `gemini`) is what Meetily uses.

The child process:

- runs in an isolated scratch directory under the app data dir (so `CLAUDE.md` / `AGENTS.md` in your projects never leak into summaries), unless you set a working directory;
- gets an extended `PATH` (Homebrew, nvm/fnm, npm global, bun, volta, `~/.local/bin`, …) because GUI apps do not inherit your shell `PATH`; as a last resort your login shell is asked where the command lives;
- is told in the prompt not to use tools or touch files;
- is killed if it exceeds the timeout (default 600 s) or when you cancel the summary.

### Presets

| Preset | Default command | Model flag |
| --- | --- | --- |
| Claude Code CLI | `claude -p --output-format text` | `--model <name>` |
| Codex CLI | `codex exec --skip-git-repo-check -s read-only -o {output_file}` | `-m <name>` |
| Gemini CLI | `gemini` | `-m <name>` |
| Custom command | *(empty)* | use `{model}` in the command |

The command is always editable. If the CLI is not found, put its absolute path in the command (`/opt/homebrew/bin/claude`, `~/.npm-global/bin/codex`, …).

Placeholders available in the command:

| Placeholder | Meaning |
| --- | --- |
| `{model}` | the optional model name from the settings (the whole flag is dropped when the model is empty) |
| `{prompt}` | the full prompt passed as a single argument instead of stdin |
| `{prompt_file}` | path of a temp file containing the prompt |
| `{output_file}` | path of a temp file the CLI should write its final answer to; if the file is non-empty it wins over stdout |

Click **Test CLI** to run the configured command once with a tiny prompt. The result shows which executable was resolved and the raw reply, which makes `PATH` and login problems obvious.

### Other agents (Antigravity, etc.)

Any agent that offers a non-interactive "print" mode can be used through the *Custom command* preset. It must read the prompt from stdin (or accept `{prompt}` / `{prompt_file}`) and print the answer to stdout (or write it to `{output_file}`). Wrapping the agent in a small shell script is often the easiest way to adapt its flags.

### Where the configuration lives

The CLI configuration is stored in the Tauri store file `cli_agent_config.json` in the app data directory. The chosen provider (`cli-agent`) is stored in the regular `settings` table like every other provider.

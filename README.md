# mafold-cli

Mafold from your terminal — a small CLI client **and** a Claude Code agent daemon.

```
mafold agent --token mb_xxxx --workdir ~/your-repo   # run Claude Code as your bot
mafold --token mb_xxxx chats                          # list your conversations
mafold --token mb_xxxx send @alice "hi there"         # send a message
```

Auth is a **bot token** (`mb_…`) — create a bot in the Mafold app — via
`--token` or `$MAFOLD_BOT_TOKEN`. Base URL defaults to `https://api.mafold.com`
(override with `--base` / `$MAFOLD_BASE`).

## Install

```sh
cd ~/your-project   # the folder the agent should work in
bash <(curl -fsSL https://raw.githubusercontent.com/mafold-lab/mafold-cli/main/install.sh) \
  agent --detach --token mb_xxxx
```

It runs in the **background** (detached from the terminal — keeps running after
you close the shell), in the current folder (add `--workdir /path` for another), logging to `~/.mafold/agent.log`. Manage it with
`mafold status` and `mafold stop`. Drop `--detach` to run in the
foreground. Or build from source: `cargo build --release`.

## `agent`

Drives the local **Claude Code** (`claude` must be installed + on PATH) in
`--workdir`: receives messages to your bot, runs Claude Code, and streams the
reply back. It always finalizes — a failure surfaces as a message, never a
stuck "typing…". The bot shows **online** only while the agent is running.

Everything runs on **your** machine, on **your** files in `--workdir`.

MIT licensed.

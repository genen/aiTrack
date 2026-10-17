# Privacy

aiTrack is a small program that runs on your own computer. It has no account, no server of its own, no analytics
and no crash reporting. **It does not send any information about you or your computer to the author or to anyone
else.**

## What it reads on your computer

- Claude and Grok usage files and settings that those tools already keep on your computer (for example the Claude
  app's cache, `~/.claude.json`, the Grok CLI's `~/.grok` folder and the Grok Bot app's usage cache).
- On a Mac, the sign-in that Claude Code stored in the macOS Keychain (macOS asks you before it is read).

aiTrack only **reads** those files. It never changes them.

## What it stores on your computer

Its own small files in `~/.aitrack` (readings, settings, which alerts were sent, a price list cache, short logs).
They never hold your sign-in token, your organisation id or your email. The full list is in the README, section
"Files and privacy". Deleting the folder resets aiTrack.

## What it sends over the network

Only these requests, over HTTPS, and only when needed:

- `api.anthropic.com`: the Claude usage question, with the Claude Code sign-in already on your computer, only when no
  file on your computer has a recent reading.
- `cli-chat-proxy.grok.com`: the Grok billing question, with the sign-in the Grok CLI stored.
- `raw.githubusercontent.com`: a download of the public model price list, about once a day.

Sign-in tokens go only to the first two hosts, are read fresh for each request, and are never written to a file or a
log by aiTrack. Starting the Grok CLI (the Refresh button) makes that CLI contact xAI as it normally does.

## Updates and contact

aiTrack does not check for updates by itself. Questions: aitrack@sirgene.com. Source and issues:
https://github.com/genen/aiTrack.

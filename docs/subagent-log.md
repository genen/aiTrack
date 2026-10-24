# Subagent log

Which model each subagent ran on. The model is taken from the harness or the session transcript, never from
memory; if it is not known the entry says "unknown".

The same entries, plus what each one was asked to do (job), its limits and its output, and the size of each run (tokens, tool calls, seconds, when the harness reports them), are kept as
one JSON object per line in `subagent-log.jsonl` (append only, newest last), so they can be charted later. Add a
line to both files for every subagent.

| Date | Subagent and purpose | Model | Source of the model |
| --- | --- | --- | --- |
| 2026-10-15 | this code agent (abbc4eec9ea3cb466): heap-corruption hunt, cache tests, About changes | claude-sonnet-5-5 | session transcript (requestedModel) |
| earlier | earlier agents (reviews, research) | unknown | not recorded; the owner recalls an Opus code review, unverified |
| 2026-10-15 | this code agent (abbc4eec9ea3cb466): fixing the flaky Windows cache tests (0.2.3-beta) | claude-sonnet-5-5 | system prompt / transcript |
| 2026-10-15 | this code agent (abbc4eec9ea3cb466): About changes, 0.2.4-beta to 0.2.5-beta | claude-sonnet-5-5 | system prompt / transcript |
| 2026-10-15 | this code agent (abbc4eec9ea3cb466): the subagent-log rule and file (0.2.5-beta) | claude-sonnet-5-5 | system prompt / transcript |
| 2026-10-15 | this code agent (abbc4eec9ea3cb466): tooltip wrapping fix and README note that the Claude app must run (0.2.6-beta) | claude-sonnet-5-5 | system prompt / transcript |
| 2026-10-15 | this code agent (abbc4eec9ea3cb466): About licence link (0.2.7-beta) | claude-sonnet-5-5 | system prompt / transcript |
| 2026-10-15 | this code agent (abbc4eec9ea3cb466): first official release 0.3.0 (version, CHANGELOG, README, CLAUDE.md) | claude-sonnet-5-5 | system prompt / transcript |
| 2026-10-15 | Plan subagent: read-only macOS port audit | claude-sonnet-5-5 | subagent transcript (model field of its messages) |
| 2026-10-15 | Mac package A subagent: Mac data dirs, Grok search dirs, Keychain login read | claude-sonnet-5-5 | subagent transcript (model field) |
| 2026-10-15 | Mac package C subagent: osascript notifications, Start at login wording | claude-sonnet-5-5 | subagent transcript (model field) |
| 2026-10-15 | Mac package B subagent: launch, start at login, raise window | claude-sonnet-5-5 | subagent transcript (model field) |
| 2026-10-15 | Mac package E subagent: app bundle, dmg, icns, macOS CI workflow | claude-sonnet-5-5 | subagent transcript (model field) |
| 2026-10-15 | CI-fix subagent: macOS/Windows test failures, tests no longer start real programs | claude-sonnet-5-5 | subagent transcript (model field) |
| 2026-10-15 | Mac package D subagent: window height fit and Mac default size | claude-sonnet-5-5 | subagent transcript (model field) |
| 2026-10-15 | Mac native notifications subagent (UNUserNotificationCenter, delegate), two rounds | claude-sonnet-5-5 | subagent transcript (model field) |
| 2026-10-16 | Docs subagent: Mac README section, changelog, version 0.3.2-beta | claude-sonnet-5-5 | subagent transcript (model field) |
| 2026-10-16 | Mac window position restore subagent (NSScreen monitor list, points-space restore) | claude-sonnet-5-5 | subagent transcript (model field) |
| 2026-10-17 | this code agent (abbc4eec9ea3cb466): README and PDF in every Windows zip (0.3.3-beta) | claude-sonnet-5-5 | system prompt / transcript |
| 2026-10-17 | Research subagent: Windows code-signing options (read-only, web) | claude-sonnet-5-5 | subagent transcript (model field) |
| 2026-10-17 | Mac signing subagent: Developer ID signing and notarization in scripts and workflow | claude-sonnet-5-5 | subagent transcript (model field) |
| 2026-10-17 | Mac zip/dmg README, README.pdf and LICENSE subagent | claude-sonnet-5-5 | subagent transcript (model field) |
| 2026-10-17 | Grok Bot weekly limit subagent: new source from the Grok Bot app cache (macOS and Windows), second bar, 0.3.5-beta | claude-sonnet-5-5 | subagent transcript (model field) |
| 2026-10-17 | Opus code review of main bd01bfd (read-only, shared/Windows/macOS code, release scripts) | claude-opus-5-5 | subagent transcript (model field) |
| 2026-10-17 | macOS review fixes subagent: alert delivery, Keychain timeout, Grok refresh group kill (0.3.8-beta) | claude-sonnet-5-5 | subagent transcript (model field) |
| 2026-10-17 | Review fixes subagent: release notes merge, poll clock, Windows no-spawn, raw whitelist, env tests (0.3.9-beta) | claude-sonnet-5-5 | subagent transcript (model field) |
| 2026-10-18 | Grok Bot weekly section on the stats page (0.3.13-beta) | claude-sonnet-5-5 | model parameter set when the subagent was launched (this session) |
| 2026-10-24 | reviewer-Nadia: Opus review of the Codex panel (PR #23, heavy on new code, light pass on the rest; read only) | claude-opus-5-5 | subagent report (harness) and model parameter set at launch |
| 2026-10-24 | coder-Tomas: show the Codex tool as ChatGPT in visible labels (0.5.8-beta) | claude-sonnet-5-5 | model parameter set when the subagent was launched (this session) |
| 2026-10-24 | coder-Ines: hold back the weekly pace until 12 h or 3 points (shared pace code) | claude-sonnet-5-5 | model parameter set when the subagent was launched (this session) |
| 2026-10-24 | reviewer-Tamsin: Opus review of the Grok refresh fixes (PR #25, heavy on changed lines; read only) | claude-opus-5-5 | model parameter set when the subagent was launched (this session) |
| 2026-10-24 | designer-Lena: ChatGPT interlaced knot badge mark (0.5.10-beta) | claude-opus-5-5 | model parameter set when the subagent was launched (this session) |

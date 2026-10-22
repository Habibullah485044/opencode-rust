---
shortcut: /mirofish <question about a simulation>
---

# MiroFish

Self-hosted multi-agent social-simulation and prediction engine (Python +
Vue, powered by CAMEL-AI's OASIS), vendored at `vendor/MiroFish` in the
workspace root (shallow clone, gitignored — never commit it into this repo).

## What it does

Feed it source material (reports, news, policy drafts, financial signals,
fiction) plus a natural-language question. It extracts information into a
graph/memory layer, spawns simulated agents with personas and memories, runs
parallel interactions, and produces a prediction report you can interrogate
follow-up.

## License boundary (important)

MiroFish is **AGPL-3.0**. Do NOT link, copy, or vendor its source into this
MIT-licensed crate. Run it as a **separate sidecar process** and talk to it
over HTTP only; AGPL obligations then stay with MiroFish itself.

## Integration status

- [x] Repo vendored: `vendor/MiroFish` (shallow clone, gitignored)
- [ ] Sidecar runner: start backend (`uv`/Docker, port 5001) from our app
- [ ] `mirofish` tool: submit material + question, poll the report
- [ ] Skill body: exact API calls once the sidecar contract is pinned

## Run it manually (upstream README)

```bash
cd vendor/MiroFish
cp .env.example .env   # add OPENAI-compatible LLM key + Zep Cloud key
npm run setup:all
npm run dev            # frontend :3000, backend :5001
```

Docker Compose works too (`docker compose up`).

## When to use

When the user asks to simulate how people/agents would react to a scenario,
forecast an outcome from source material, or says "mirofish". Source
material from the web should be fetched first (our tools), then submitted —
MiroFish does not crawl the internet itself.

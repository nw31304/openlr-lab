# LLM System Prompt

`SYSTEM_PROMPT.md` is the source of truth for the AI chat system prompt.
Edit it freely — plain markdown, no special syntax required.

## Updating the prompt

1. Edit `SYSTEM_PROMPT.md`
2. Optionally ask an LLM to review or improve it (see below)
3. Regenerate the JS module:
   ```
   npm run build:prompt
   ```
4. Commit both `SYSTEM_PROMPT.md` and `systemPrompt.js`

## Asking an LLM to improve the prompt

Paste the contents of `SYSTEM_PROMPT.md` into a capable model (Claude, GPT-4o)
with a prompt like:

> You are reviewing a system prompt for an OpenLR decode diagnostic assistant.
> Improve clarity, fix any inaccuracies, and add a worked example for [failure mode].
> Keep the existing structure. Return only the revised markdown.

Copy the response back into `SYSTEM_PROMPT.md`, then run step 3.

## Files

| File | Purpose |
|---|---|
| `SYSTEM_PROMPT.md` | Source — edit this |
| `build-prompt.js` | Generator script |
| `systemPrompt.js` | Generated — do not edit directly |

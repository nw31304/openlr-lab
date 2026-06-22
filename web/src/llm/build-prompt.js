#!/usr/bin/env node
/**
 * Bakes SYSTEM_PROMPT.md into systemPrompt.js.
 *
 * Usage:  node src/llm/build-prompt.js
 *    or:  npm run build:prompt
 *
 * Edit SYSTEM_PROMPT.md freely (plain markdown), then run this script.
 * Commit both the .md source and the generated .js — the app works without
 * running the script, and the .md is the authoritative source to hand to an
 * LLM for review and improvement.
 */

import { readFileSync, writeFileSync } from 'node:fs';
import { resolve, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const __dirname = dirname(fileURLToPath(import.meta.url));

const mdPath  = resolve(__dirname, 'SYSTEM_PROMPT.md');
const outPath = resolve(__dirname, 'systemPrompt.js');

const markdown = readFileSync(mdPath, 'utf8').trimEnd();

// Escape characters that would break a JS template literal
const escaped = markdown
  .replace(/\\/g, '\\\\')   // backslashes first
  .replace(/`/g,  '\\`')    // backticks
  .replace(/\$\{/g, '\\${'); // template expressions

const output =
`// AUTO-GENERATED — do not edit directly.
// Source:      src/llm/SYSTEM_PROMPT.md
// Regenerate:  node src/llm/build-prompt.js  (or: npm run build:prompt)
export const SYSTEM_PROMPT = \`${escaped}\`;
`;

writeFileSync(outPath, output, 'utf8');

const lines = markdown.split('\n').length;
const bytes = Buffer.byteLength(output, 'utf8');
console.log(`✓ systemPrompt.js  (${lines} lines of markdown → ${bytes} bytes)`);

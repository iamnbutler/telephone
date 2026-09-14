#!/usr/bin/env node
// Next Cloud uses the signed-in GitHub CLI account; no token is stored here.
import { execFileSync } from 'node:child_process';
import { readFileSync, statSync } from 'node:fs';
import { basename, extname } from 'node:path';

const base = 'https://next-cloud.githubnext.com';
const [command, input] = process.argv.slice(2);

async function main() {
  let path;
  let method = 'GET';
  let body;
  let contentType;

  if (command === 'get' && input?.startsWith('/api/')) {
    path = input;
  } else if (command === 'upload' && input) {
    const types = {
      '.mp4': 'video/mp4', '.webm': 'video/webm',
      '.png': 'image/png', '.jpg': 'image/jpeg', '.jpeg': 'image/jpeg',
      '.webp': 'image/webp', '.gif': 'image/gif',
    };
    contentType = types[extname(input).toLowerCase()];
    if (!contentType) throw new Error('Unsupported media extension.');
    if (statSync(input).size > 200_000_000) throw new Error('Upload exceeds 200 MB.');
    path = `/api/media?filename=${encodeURIComponent(basename(input))}`;
    method = 'POST';
    body = readFileSync(input);
  } else if (command === 'post' && input) {
    const post = JSON.parse(readFileSync(input, 'utf8'));
    if (!post.body && !post.artifact) throw new Error('A body or artifact is required.');
    path = '/api/posts';
    method = 'POST';
    body = JSON.stringify(post);
    contentType = 'application/json';
  } else {
    throw new Error('Usage: node scripts/next-cloud.mjs get /api/... | upload FILE | post POST.json');
  }

  const url = new URL(path, base);
  if (url.origin !== base || !url.pathname.startsWith('/api/')) {
    throw new Error('Only the Next Cloud API is supported.');
  }
  const token = execFileSync('gh', ['auth', 'token'], { encoding: 'utf8' }).trim();
  const response = await fetch(url, {
    method,
    headers: {
      Authorization: `Bearer ${token}`,
      ...(contentType ? { 'Content-Type': contentType } : {}),
    },
    body,
    redirect: 'error',
    signal: AbortSignal.timeout(180_000),
  });
  const text = await response.text();
  if (!response.ok) throw new Error(`Next Cloud HTTP ${response.status}: ${text}`);
  console.log(JSON.stringify(JSON.parse(text), null, 2));
}

main().catch(error => {
  console.error(error.message);
  process.exitCode = 1;
});

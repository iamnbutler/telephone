# Posting Telephone updates to Next Cloud

Use [Next Cloud's API](https://next-cloud.githubnext.com/api-docs) to publish
meaningful demos and progress updates. The helper requires Node.js 18+ and the
GitHub CLI signed in as the person publishing:

```sh
gh auth status
node scripts/next-cloud.mjs get /api/projects
```

The helper obtains the GitHub token at request time and sends it only to
`https://next-cloud.githubnext.com/api/`. It does not save the token or follow
redirects.

## Upload and publish

Upload local media first (raw bytes, up to 200 MB):

```sh
node scripts/next-cloud.mjs upload /absolute/path/to/demo.mp4
```

Prepare a JSON file with the returned media URL, a brief description, and any
known media dimensions. Credit the generating agent separately from the human
authors. For example:

```json
{
  "title": "Telephone: Let your agents chat",
  "body": "What changed, what the demo shows, and why it matters.",
  "artifact": "https://example.com/replace-with-uploaded-demo.mp4",
  "alt": "A description of the demo video.",
  "width": 2560,
  "height": 1410,
  "durationMs": 201733,
  "generator": "Codex",
  "generatorModel": "gpt-6-astra"
}
```

Then publish the file:

```sh
node scripts/next-cloud.mjs post /absolute/path/to/post.json
```

This publishes immediately unless the payload includes `"draft": true`.
Only publish when the user requests it. Do not post for every commit.
`project` is optional; use an existing slug or exact name from `/api/projects`.
An unknown project is rejected. Omit `publishedAt` to publish with the current
time. At least one of `body` and `artifact` is required.

Verify the returned post ID with `get /api/posts/POST_ID` and open its returned
URL to check the rendered video and text. If a publish request times out,
inspect recent posts before retrying so the update is not duplicated.

## Published demo

[Telephone: Let your agents chat](https://next-cloud.githubnext.com/iamnbutler/telephone-let-your-agents-chat)
was published on September 14, 2026 with the narrated Hob review demo.
Post ID: `pst_d4znn13v2n56w4hg`. Generator credit: Codex (`gpt-6-astra`).

---
id: markdown-design
kind: design
title: Markdown card
---

## Release

### Rollback

Scale $x^2$ before rollout.

- Check **status** and `state`.
  1. Save the *current* settings.
  2. Restore the previous version.

| Stage | Result | Operator notes |
| --- | --- | --- |
| Prepare | Ready | Check the current version and keep a copy of its configuration. |
| Restore | Pending | Wait for startup and verify the previous version again. |

> Keep the previous version available.

```sh
printf '%s\n' "$HOME"
printf '%s\n' '[[not-a-link]]'
```

See [the guide](https://example.test/guide), [[markdown-design|this card]], and [[file:papers/reader/deck.toml#L2|the deck]].

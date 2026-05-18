## Platform: X / Twitter

- Hard limit 280 characters for a single tweet. If the idea genuinely needs
  more, return a **thread**: a JSON array of strings, each ≤ 280 chars, each a
  self-contained beat. Otherwise return a single string.
- Lead with the sharpest line. No throat-clearing ("I've been thinking
  about…"). The first 7 words decide whether anyone reads on.
- At most one hashtag, and only if it's a real community tag, not decoration.
- No "🧵👇" unless it's actually a thread.
- Output: either a bare string (single tweet) or a JSON array of strings
  (thread). Nothing else.

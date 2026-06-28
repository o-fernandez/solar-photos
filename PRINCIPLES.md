# Responsiveness Principles

> These are **law.** Every feature must honor them. Responsiveness is not a
> nice-to-have here — it is the product. See [VISION.md](VISION.md) for the why.

1. **Never steal the user's focus.** Indexing, thumbnail generation, face
   detection, and any background work must run off the UI thread and must never
   grab focus, open dialogs, or interrupt what the user is doing.

2. **Never reflow what the user is looking at.** New thumbnails, detected faces,
   or indexed items must stream in without making the visible grid jump, scroll,
   or lose the user's place. If new data arrives for an off-screen area, it waits
   silently. The user's current position is sacred.

3. **Foreground always wins.** If the user starts interacting, background work
   yields. The thing the user is actively doing gets priority over everything
   the app wants to do.

4. **Cold start must be instant on repeat launches.** All thumbnails and index
   data are cached to disk. A second launch of a large library shows content
   immediately — never a multi-second "loading your photos" wall.

5. **Automate the tedious, confirm in batches.** For anything requiring user
   judgment (like naming faces), the app does the clustering work itself, then
   asks for confirmation in batches. Never make the user hunt through photos to
   do work the software should have done.

6. **Degrade gracefully at scale.** Every design decision assumes a 100,000+
   photo library, not a 1,000-photo demo. If something feels fine at 1k but
   janky at 100k, it's wrong.

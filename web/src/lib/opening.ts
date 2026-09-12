/**
 * Who owns the loading flag while a chapter is being opened.
 *
 * Opening a chapter is a chain of awaits - the held shard, the chapter endpoint,
 * the shard fetch, then the manifest - and at any point along it another open
 * can start: the reader taps a row in the drawer, a live `position` event
 * follows another device, a book finishes loading and jumps to the saved place.
 * So two of these run at once and both want to say whether the reading view is
 * still waiting.
 *
 * The old rule compared the book and chapter refs on the way out:
 *
 *     if (bookRef.current !== b || ciRef.current !== target) return false;
 *
 * which correctly stops a superseded open from painting the wrong words, but
 * returns *without clearing the flag* - and it cannot simply clear it either,
 * because the open that superseded it may still be in flight and the flag is
 * now that one's to hold. Two wrongs were available and it picked one: leave
 * the reading view on its skeleton forever, with no words and no message.
 *
 * The rule that has neither failure is a generation token. Every open takes one
 * and the newest wins:
 *
 *   - `current()` is false for an open that has been superseded, so it paints
 *     nothing and touches nothing;
 *   - `settle()` clears the flag only for the newest open, so a straggler cannot
 *     clobber the state of the one that replaced it;
 *   - and because the newest open always settles, the flag is always released
 *     by exactly one of them, whichever order they finish in.
 *
 * The one deliberate exception is state.tsx's optimistic `cacheOnly` first
 * paint, which returns on a miss *without* settling: the real open is already on
 * its way behind it and owns the wait. That is why this is a seam rather than a
 * rule buried in a component - the invariant is worth a test.
 */
export interface OpenAttempt {
  /** Is this still the open the reading view belongs to? */
  current(): boolean;
  /** Finished, however it went. A superseded attempt releases nothing. */
  settle(): void;
}

export interface OpenSequence {
  begin(): OpenAttempt;
}

export function openSequence(setLoading: (on: boolean) => void): OpenSequence {
  let latest = 0;
  return {
    begin(): OpenAttempt {
      const mine = ++latest;
      setLoading(true);
      return {
        current: () => latest === mine,
        settle: () => { if (latest === mine) setLoading(false); },
      };
    },
  };
}

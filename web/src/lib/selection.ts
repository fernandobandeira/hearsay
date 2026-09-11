/**
 * Chapter selection, as a mode rather than a column of checkboxes.
 *
 * The old drawer put a 14 px checkbox on every row and three verbs under the
 * list. On a phone that is a miss-tap generator, and it also meant the row's
 * *own* action - jump to this chapter - had to share the row with a control
 * nobody could hit. So selection became a mode:
 *
 *   outside a mode   a tap on a row opens that chapter. That is the obvious
 *                    thing and it is what the row looked like it did all along.
 *   inside a mode    a tap on a row toggles it, the whole row is the target,
 *                    and the only way out is confirm or cancel.
 *
 * The mode also carries *which* verb is being aimed, because that decides which
 * rows can be picked at all: "download" cannot touch a chapter already on the
 * device, "remove" cannot touch one that is not. Eligibility is the caller's -
 * it knows the rows - but it is threaded through every event here so a stale
 * pick can never survive into the confirm step.
 *
 * Kept out of the component because all three of the interesting rules (an
 * ineligible pick is dropped, a bulk range replaces rather than accumulates,
 * cancelling forgets) are exactly the kind that regress silently.
 */

export type SelectVerb = 'download' | 'remove';

export interface SelectionState {
  /** null is "not selecting anything": rows jump instead of toggling. */
  verb: SelectVerb | null;
  picked: ReadonlySet<number>;
}

export const idle: SelectionState = {verb: null, picked: new Set()};

export type SelectionEvent =
  | {t: 'start'; verb: SelectVerb}
  /** one row tapped; `eligible` is the rows the current verb may act on */
  | {t: 'toggle'; ci: number; eligible: readonly number[]}
  /** a bulk option: replaces the picks rather than adding to them */
  | {t: 'pick'; cis: readonly number[]; eligible: readonly number[]}
  | {t: 'clear'}
  | {t: 'cancel'};

export function reduce(s: SelectionState, e: SelectionEvent): SelectionState {
  switch (e.t) {
    case 'start':
      // Re-starting the same verb is a no-op, not a reset: the primary buttons
      // are also what a returning tap lands on.
      return s.verb === e.verb ? s : {verb: e.verb, picked: new Set()};
    case 'toggle': {
      if (!s.verb) return s;
      if (!e.eligible.includes(e.ci)) return s;
      const picked = new Set(s.picked);
      picked.has(e.ci) ? picked.delete(e.ci) : picked.add(e.ci);
      return {verb: s.verb, picked};
    }
    case 'pick': {
      if (!s.verb) return s;
      const ok = new Set(e.eligible);
      return {verb: s.verb, picked: new Set(e.cis.filter((c) => ok.has(c)))};
    }
    case 'clear':
      return s.verb ? {verb: s.verb, picked: new Set()} : s;
    case 'cancel':
      return idle;
  }
}

/** The picks, in chapter order - what the confirm step actually runs on. */
export function chosen(s: SelectionState): number[] {
  return [...s.picked].sort((a, b) => a - b);
}

/**
 * The bulk options: "the next N chapters after the one being read", and
 * "everything after it" when `n` is null.
 *
 * `ordered` is the eligible chapter indices in reading order - already filtered
 * by the verb, so "next 10 to download" skips the ten he already has and offers
 * the next ten that need it. Strictly *after* `current`: the chapter in front
 * of his eyes is the one he can reach by tapping its own row, and quietly
 * including it would make "next 5" mean six.
 */
export function rangeAfter(
  ordered: readonly number[], current: number | null | undefined, n: number | null,
): number[] {
  const after = ordered.filter((ci) => current == null || ci > current);
  return n == null ? [...after] : after.slice(0, Math.max(0, n));
}

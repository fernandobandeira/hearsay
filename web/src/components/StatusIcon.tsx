/**
 * The drawer's one icon vocabulary, shared by the chapter rows and the book rows.
 *
 * A row used to spell its state out ("rendered", "208/1433", "7.8MB"); on a
 * phone that was a second column of small print beside every title, and the
 * words said less than a picture would. So each row carries exactly one mark on
 * its right edge, and the sentence lives in the tooltip and the aria-label:
 *
 *   solid disc   on this device             (ok)
 *   cloud        packed on the server, can be downloaded
 *   ring         being made - the arc is how much of it exists
 *   spinner      packing, or copying onto this device
 *   clock        waiting its turn
 *   check        rendered, not packed yet   (subtle)
 *   nothing      nothing rendered
 *
 * The mapping from a state key to a mark is here rather than in lib/, because it
 * is presentation: lib/chapterstate.ts and lib/library.ts still decide *which*
 * state a row is in, and the tests that pin that down are unchanged.
 */
import {memo} from 'react';
import {ArrowDown, Check, Clock, Cloud, Loader2} from 'lucide-react';
import {cn} from '@/lib/utils';

export type Mark = 'device' | 'cloud' | 'ring' | 'spin' | 'wait' | 'check' | 'none';

export const TONE = {
  ok: 'text-ok', work: 'text-work', part: 'text-part',
  done: 'text-foreground/45', none: 'text-muted-foreground/70',
} as const;
export type Tone = keyof typeof TONE;

/**
 * A 12 px progress ring. `frac` is clamped to (0, 1) so that a started ring
 * always shows an arc and an unfinished one never closes - the same rule
 * `renderedPct` follows for the book's percentage.
 */
export function Ring({frac, className}: {frac: number; className?: string}) {
  const r = 4.5;
  const c = 2 * Math.PI * r;
  const f = Math.min(0.96, Math.max(0.06, Number.isFinite(frac) ? frac : 0));
  return (
    <svg viewBox="0 0 12 12" className={cn('size-3', className)} aria-hidden>
      <circle cx="6" cy="6" r={r} fill="none" stroke="currentColor" strokeOpacity={0.22} strokeWidth={1.5} />
      <circle cx="6" cy="6" r={r} fill="none" stroke="currentColor" strokeWidth={1.5}
              strokeLinecap="round" strokeDasharray={`${f * c} ${c}`}
              transform="rotate(-90 6 6)" />
    </svg>
  );
}

/** A filled disc with the arrow knocked out of it: the one solid mark. */
function OnDevice({className}: {className?: string}) {
  return (
    <span className={cn('relative flex size-3 items-center justify-center rounded-full bg-current', className)}
          aria-hidden>
      <ArrowDown className="size-2.5 text-background" strokeWidth={3} />
    </span>
  );
}

export const StatusIcon = memo(function StatusIcon({mark, tone, frac = 0, label, testId, phase}: {
  mark: Mark;
  tone: Tone;
  /** only read for `ring` */
  frac?: number;
  label: string;
  testId?: string;
  phase?: string;
}) {
  const cls = cn('flex size-4 shrink-0 items-center justify-center', TONE[tone]);
  // "nothing rendered" draws nothing, but keeps its slot so the titles of a
  // list of mixed rows all end at the same x.
  if (mark === 'none') {
    return <span data-testid={testId} data-phase={phase} className={cls} title={label} aria-label={label} />;
  }
  return (
    <span data-testid={testId} data-phase={phase} role="img" aria-label={label} title={label} className={cls}>
      {mark === 'device' && <OnDevice />}
      {mark === 'cloud' && <Cloud className="size-3.5" strokeWidth={1.75} />}
      {mark === 'ring' && <Ring frac={frac} />}
      {mark === 'spin' && <Loader2 className="size-3 animate-spin" />}
      {mark === 'wait' && <Clock className="size-3" strokeWidth={1.75} />}
      {mark === 'check' && <Check className="size-3" strokeWidth={2} />}
    </span>
  );
});

/**
 * A hairline progress bar. `value` is 0..1. Used under the current chapter
 * (where in it the reader is), under the whole-book line, and under a book row
 * (how far into it the stored position is).
 */
export function Hairline({value, className, barClassName, testId}: {
  value: number; className?: string; barClassName?: string; testId?: string;
}) {
  const v = Math.min(1, Math.max(0, Number.isFinite(value) ? value : 0));
  return (
    <span data-testid={testId} role="progressbar" aria-valuemin={0} aria-valuemax={100}
          aria-valuenow={Math.round(v * 100)}
          className={cn('block h-px w-full overflow-hidden bg-white/[0.07]', className)}>
      <span className={cn('block h-full bg-foreground/45', barClassName)}
            style={{width: `${v * 100}%`}} />
    </span>
  );
}

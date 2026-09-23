/**
 * The reading view - which is now the only view.
 *
 * There is no "read with your eyes" mode, because that was never a mode: not
 * playing anything *is* reading. So this is the page, always, and what playback
 * adds is following. The rules, all of them from lib/reading.ts:
 *
 *   the active chunk is centred, not pinned to the top;
 *   it auto-scrolls only when playback moves it, never on a click or a re-render;
 *   scrolling away stops the following and offers it back, rather than fighting;
 *   clicking a chunk sets the position, and nothing quietly undoes that.
 *
 * The chapter's own title is not repeated here: the book's text already opens
 * with its heading, and the bottom bar carries it for the chapters that do not.
 *
 * The box is the whole page - both bars overlay it - so it carries their
 * measured heights as its own padding (`--reading-top`, `--bar-bottom`, see the
 * scaffold comment in index.css). A chapter therefore begins and ends clear of
 * them, and everything in between scrolls under a bar that is about to fade.
 */
import {useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState} from 'react';
import {ArrowDownToLine} from 'lucide-react';
import {Skeleton} from '@/components/ui/skeleton';
import {centerTop, isVisible, scrollPick, type Span} from '@/lib/reading';
import {cn} from '@/lib/utils';

/** How much of the viewport's edge does not count as "in front of the eyes". */
const EDGE = 48;

export function ReadingPage({
  chunks, paras, idx, fontScale, onPick, playing, nextTitle, onNext, loading,
}: {
  chunks: string[];
  paras: number[] | null;
  idx: number;
  fontScale: number;
  onPick: (i: number) => void;
  /** true while audio is driving the position: follow it instead of setting it */
  playing: boolean;
  nextTitle: string | null;
  onNext: () => void;
  loading: boolean;
}) {
  const box = useRef<HTMLDivElement>(null);
  const spans = useRef<(HTMLSpanElement | null)[]>([]);
  /** programmatic scrolls must not be mistaken for the reader's own */
  const quiet = useRef(0);
  const lastIdx = useRef(idx);
  const clicked = useRef(false);
  const settle = useRef<ReturnType<typeof setTimeout> | null>(null);
  const [following, setFollowing] = useState(true);

  const blocks = useMemo(() => {
    const out: {para: number; from: number; to: number}[] = [];
    for (let i = 0; i < chunks.length; i++) {
      const p = paras ? paras[i] : i;
      const last = out[out.length - 1];
      if (last && last.para === p) last.to = i;
      else out.push({para: p, from: i, to: i});
    }
    return out;
  }, [chunks, paras]);

  const measure = useCallback((i: number): Span | null => {
    const el = spans.current[i];
    return el ? {top: el.offsetTop, height: el.offsetHeight} : null;
  }, []);

  const scrollTo = useCallback((i: number, behavior: ScrollBehavior) => {
    const b = box.current;
    const span = measure(i);
    if (!b || !span) return;
    quiet.current = performance.now() + (behavior === 'smooth' ? 800 : 250);
    b.scrollTo({top: centerTop(span, {viewHeight: b.clientHeight,
                                      maxScroll: b.scrollHeight - b.clientHeight}), behavior});
  }, [measure]);

  // A new chapter opens at its saved position, centred, with no animation: this
  // is the resume, and it should look like the page was always there.
  useLayoutEffect(() => {
    if (!chunks.length) return;
    setFollowing(true);
    lastIdx.current = idx;
    scrollTo(idx, 'auto');
    // Fonts and images settle a frame later; re-centre once on the same position.
    const t = setTimeout(() => scrollTo(idx, 'auto'), 60);
    return () => clearTimeout(t);
    // Deliberately only on a chapter change - idx changes are the effect below.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [chunks]);

  // Playback moved the active chunk: centre it, unless the reader has scrolled
  // away (they get the follow button instead) or put it there themselves.
  useEffect(() => {
    const moved = lastIdx.current !== idx;
    lastIdx.current = idx;
    if (!moved) return;
    if (clicked.current) { clicked.current = false; return; }
    if (!playing || !following) return;
    scrollTo(idx, 'smooth');
  }, [idx, playing, following, scrollTo]);

  // Starting playback is an explicit "take me back to the audio".
  useEffect(() => {
    if (!playing) return;
    setFollowing(true);
    scrollTo(idx, 'smooth');
    // only when playback starts
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [playing]);

  function onScroll() {
    if (performance.now() < quiet.current) return;
    const b = box.current;
    if (!b) return;
    const view = {scrollTop: b.scrollTop, viewHeight: b.clientHeight};

    // Playback: the reader is in charge of the scroll, so stop following them
    // around the moment what is playing is no longer on the screen.
    if (playing && following) {
      const cur = measure(idx);
      if (cur && !isVisible(cur, view, EDGE)) setFollowing(false);
      return;
    }
    if (playing) return;

    // Paused: the position follows the eyes, but only once the scroll settles and
    // only when it has actually left the chunk behind (bug 4).
    if (settle.current) clearTimeout(settle.current);
    settle.current = setTimeout(() => {
      const el = box.current;
      if (!el) return;
      const next = scrollPick({
        active: idx,
        spans: chunks.map((_, i) => measure(i)),
        view: {scrollTop: el.scrollTop, viewHeight: el.clientHeight},
        playing: false,
        pad: EDGE,
      });
      if (next != null) { clicked.current = true; onPick(next); }
    }, 320);
  }

  useEffect(() => () => { if (settle.current) clearTimeout(settle.current); }, []);

  function pick(i: number) {
    clicked.current = true;      // a click sets the position; it does not scroll
    if (settle.current) clearTimeout(settle.current);
    onPick(i);
  }

  const size = Math.round(Math.max(16.5, Math.min(21, (box.current?.clientWidth ?? 700) * 0.0155)) * fontScale);

  if (loading && !chunks.length) return <ChapterSkeleton />;

  return (
    <>
      <div
        ref={box}
        data-testid="reading"
        onScroll={onScroll}
        className="absolute inset-0 overflow-y-auto overflow-x-hidden
                   pt-[var(--reading-top)] pb-[calc(var(--bar-bottom)+var(--bar-fade))]
                   [-webkit-overflow-scrolling:touch] [overscroll-behavior:contain]"
        style={{fontSize: `${size}px`}}
      >
        {blocks.map((b) => (
          <p key={b.from}
             className="mx-auto mb-[1.15em] max-w-[min(40em,92vw)] px-[max(18px,3vw)]
                        font-light leading-[1.62] tracking-[-0.004em] text-foreground/85">
            {Array.from({length: b.to - b.from + 1}, (_, n) => {
              const i = b.from + n;
              return (
                <span
                  key={i}
                  data-testid="chunk"
                  data-i={i}
                  data-active={i === idx ? '1' : undefined}
                  ref={(el) => { spans.current[i] = el; }}
                  onClick={() => pick(i)}
                  className={cn(
                    'cursor-pointer rounded-[3px] transition-colors',
                    'hover:bg-white/[0.06]',
                    i === idx && 'bg-white/[0.09] text-foreground shadow-[0_0_0_3px_rgba(255,255,255,0.08)]',
                  )}
                >
                  {chunks[i]}{' '}
                </span>
              );
            })}
          </p>
        ))}
        <div className="mx-auto my-10 max-w-[min(40em,92vw)] px-[max(18px,3vw)] text-xs text-muted-foreground">
          {nextTitle
            ? <button onClick={onNext} className="border-b border-border pb-0.5 hover:text-foreground">
                next: {nextTitle}
              </button>
            : 'end of the book'}
        </div>
        {/* Run-out, so the *last* chunk can still be centred like every other
            one. It used to be `pb-[45vh]`, which was 45 % of the whole screen
            measured against a box that is now the screen minus two bars - a
            fifth of a phone screen of guaranteed blank at the end of every
            chapter. A percentage of the reading box is the same rule stated
            against the thing it is actually padding. */}
        <div aria-hidden className="h-[45%]" />
      </div>

      {playing && !following && (
        <button
          data-testid="follow"
          onClick={() => { setFollowing(true); scrollTo(idx, 'smooth'); }}
          className="absolute bottom-[calc(var(--bar-bottom)+0.5rem)] left-1/2 z-10 flex
                     -translate-x-1/2 items-center gap-1.5
                     rounded-full border border-border bg-card/90 px-3 py-1.5 text-[11px]
                     text-muted-foreground shadow-lg backdrop-blur transition-colors
                     hover:text-foreground"
        >
          <ArrowDownToLine className="size-3" /> follow the narration
        </button>
      )}
    </>
  );
}

/** The chapter's words are on their way. Never a blank screen. */
export function ChapterSkeleton() {
  const widths = ['96%', '88%', '92%', '70%', '94%', '86%', '90%', '64%', '93%', '82%', '88%', '48%'];
  return (
    <div data-testid="chapter-skeleton"
         className="absolute inset-0 overflow-hidden pt-[var(--reading-top)]">
      <div className="mx-auto max-w-[min(40em,92vw)] space-y-3 px-[max(18px,3vw)]">
        {widths.map((w, i) => (
          <Skeleton key={i} className="h-4 bg-white/[0.045]" style={{width: w}} />
        ))}
      </div>
    </div>
  );
}

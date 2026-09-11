/**
 * The chrome: the top bar and the player bar.
 *
 * Both are rows of `#root`'s flex column now, not `position: fixed` overlays -
 * see the scaffold comment in index.css for why. Each spends one safe-area
 * inset, the one on the edge it touches, and neither states a height: the
 * reading area is whatever is left over, so there is no number to drift.
 *
 * They hide themselves when nothing has moved for two seconds, because a reader
 * should be the words and nothing else - and come back on any input. Hiding is
 * opacity, not layout: reflowing a page of text every couple of seconds would
 * be worse than the bar.
 *
 * The top bar carries controls and nothing else. It used to also carry a
 * diagnostics line ("streaming · buffer 15 · RTF 4.7× · 2.5h cached ·
 * 0.37/50 GB") and a chunk counter ("72/87"), which is a readout of the render
 * pipeline on the one surface that is on screen while reading. Both are gone.
 * What survives on the right, in the order a thumb reaches them: play/pause at
 * the very edge, the mic beside it, and `T` - which swaps the whole row for the
 * text-size slider until its X puts the controls back. The only text the bar
 * still shows is an error, because an error is not a diagnostic.
 */
import {useEffect, useState} from 'react';
import {
  ChevronLeft, ChevronRight, CloudOff, Loader2, Menu, Pause, Play, Type, X,
} from 'lucide-react';
import {Button} from '@/components/ui/button';
import {Slider} from '@/components/ui/slider';
import {Badge} from '@/components/ui/badge';
import {cn} from '@/lib/utils';
import {useNarrator} from '@/state';
import {VoiceNote} from './VoiceNote';

export function useAutoHide() {
  const [visible, setVisible] = useState(true);
  useEffect(() => {
    let t: ReturnType<typeof setTimeout>;
    const poke = () => {
      setVisible(true);
      clearTimeout(t);
      t = setTimeout(() => setVisible(false), 2400);
    };
    for (const e of ['mousemove', 'mousedown', 'wheel', 'touchstart', 'keydown'])
      window.addEventListener(e, poke, {passive: true});
    poke();
    return () => {
      clearTimeout(t);
      for (const e of ['mousemove', 'mousedown', 'wheel', 'touchstart', 'keydown'])
        window.removeEventListener(e, poke);
    };
  }, []);
  return visible;
}

export function TopBar({visible, onMenu}: {visible: boolean; onMenu: () => void}) {
  const n = useNarrator();
  /* Text size is a thing you set once and then forget, so it does not deserve a
     permanent seat - but it also must not be a drawer away while reading. `T`
     borrows the bar for as long as it takes. */
  const [sizing, setSizing] = useState(false);
  const disabled = !n.chunks.length;
  return (
    <header
      data-testid="topbar"
      className={cn(
        // The top inset, spent once. max() because a device without a notch
        // still wants the row off the very edge of the glass.
        'z-20 shrink-0 bg-background pt-[max(0.25rem,var(--sat))]',
        'transition-opacity duration-300',
        visible ? 'opacity-100' : 'pointer-events-none opacity-0',
      )}
    >
      <div className="flex h-11 items-center gap-1 px-2">
        {sizing ? (
          <TextSize onClose={() => setSizing(false)} />
        ) : (
          <>
            <Button data-testid="menu" variant="ghost" size="icon" onClick={onMenu}
                    title="Library (Esc closes)"
                    className="size-9 shrink-0 rounded-full text-muted-foreground hover:text-foreground">
              <Menu className="size-4.5" />
            </Button>

            {/* The only words on the bar, and only when something is wrong. */}
            <div className="min-w-0 flex-1">
              {n.message && (
                <span data-testid="bar-message"
                      className="block truncate text-[11px] leading-tight text-destructive">
                  {n.message}
                </span>
              )}
            </div>

            <Connection />

            <Button data-testid="text-size" variant="ghost" size="icon"
                    onClick={() => setSizing(true)} title="Reading text size"
                    className="size-9 shrink-0 rounded-full text-muted-foreground hover:text-foreground">
              <Type className="size-4" />
            </Button>
            <VoiceNote disabled={disabled} />
            <Button data-testid="play" variant="ghost" size="icon" disabled={disabled}
                    onClick={n.toggle} title="Play / pause (Space)"
                    className="size-9 shrink-0 rounded-full text-muted-foreground hover:text-foreground">
              {n.playing ? <Pause className="size-4.5" /> : <Play className="size-4.5" />}
            </Button>
          </>
        )}
      </div>
    </header>
  );
}

/** The bar, borrowed. One slider, one way out, no other controls to mis-tap. */
function TextSize({onClose}: {onClose: () => void}) {
  const n = useNarrator();
  const pct = Math.round(n.fontScale * 100);
  return (
    <>
      <Type className="ml-2 size-4 shrink-0 text-muted-foreground" />
      <Slider data-testid="font-slider" className="min-w-0 flex-1" min={70} max={180} step={5}
              value={[pct]}
              onValueChange={([v]) => n.setFontScale(v / 100)}
              aria-label="Reading text size" />
      <span className="w-10 shrink-0 text-right text-[11px] tabular-nums text-muted-foreground">
        {pct}%
      </span>
      <Button data-testid="text-size-close" variant="ghost" size="icon" onClick={onClose}
              title="Done"
              className="size-9 shrink-0 rounded-full text-muted-foreground hover:text-foreground">
        <X className="size-4.5" />
      </Button>
    </>
  );
}

function Connection() {
  const n = useNarrator();
  const {notes, positions, stalled} = n.queued;
  const quiet = n.conn === 'online' && !notes && !positions;
  if (quiet) return null;
  const text = n.conn === 'reconnecting' ? 'reconnecting…' : [
    n.conn === 'offline' ? 'offline' : null,
    notes ? `${notes} note${notes > 1 ? 's' : ''} queued` : null,
    stalled ? `${stalled} stalled` : null,
    positions ? 'position queued' : null,
  ].filter(Boolean).join(' · ');
  return (
    <Badge
      data-testid="conn"
      variant="outline"
      onClick={() => void n.flush(true)}
      title="Queued voice notes and positions — tap to retry now"
      className={cn('shrink-0 cursor-pointer gap-1 font-normal',
                    n.conn === 'offline' && 'border-destructive/40 text-destructive',
                    n.conn === 'reconnecting' && 'border-part/40 text-part')}
    >
      {n.conn === 'offline' ? <CloudOff className="size-3" />
        : n.conn === 'reconnecting' ? <Loader2 className="size-3 animate-spin" /> : null}
      {text}
    </Badge>
  );
}

export function PlayerBar({visible}: {visible: boolean}) {
  const n = useNarrator();
  const pct = n.chunks.length > 1 ? (n.idx / (n.chunks.length - 1)) * 100 : 0;
  return (
    <footer
      data-testid="playerbar"
      className={cn(
        // The bottom inset, spent once, as max() - the ~34 px home-indicator
        // strip already is the breathing room, and 0.5rem is the floor for a
        // device that has none. Adding the two is how the band appears.
        'z-20 shrink-0 bg-background px-4 pb-[max(0.5rem,var(--sab))] pt-1',
        'transition-opacity duration-300',
        visible ? 'opacity-100' : 'pointer-events-none opacity-0',
      )}
    >
      <Slider
        className="mb-2" min={0} max={Math.max(0, n.chunks.length - 1)} step={1}
        value={[n.idx]}
        onValueChange={([v]) => n.setIdx(v)}
        disabled={!n.chunks.length}
        aria-label="Position in the chapter"
      />
      <div data-testid="chapter-nav" className="flex items-center gap-2">
        <Button variant="ghost" size="icon" disabled={n.ci <= 0} onClick={() => n.goChapter(-1)}
                title="Previous chapter (←)"
                className="size-8 rounded-full text-muted-foreground hover:text-foreground">
          <ChevronLeft className="size-4" />
        </Button>
        <div className="min-w-0 flex-1 truncate text-center text-xs text-muted-foreground">
          {n.chapterTitle || 'no chapter'}
          <span className="ml-2 opacity-60">{pct.toFixed(0)}%</span>
        </div>
        <Button variant="ghost" size="icon"
                disabled={n.ci >= n.chapters.length - 1} onClick={() => n.goChapter(1)}
                title="Next chapter (→)"
                className="size-8 rounded-full text-muted-foreground hover:text-foreground">
          <ChevronRight className="size-4" />
        </Button>
      </div>
    </footer>
  );
}

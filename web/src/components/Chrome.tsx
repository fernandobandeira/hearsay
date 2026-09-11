/**
 * The chrome: the top bar, the progress track and the diagnostics line.
 *
 * It hides itself when nothing has moved for two seconds, because a reader should
 * be the words and nothing else - and comes back on any input.
 */
import {useEffect, useState} from 'react';
import {
  ChevronLeft, ChevronRight, CloudOff, Loader2, Menu, Pause, Play, Type,
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
  const disabled = !n.chunks.length;
  return (
    <div className={cn(
      // Safe-area aware: the bar's background extends under the iOS notch, the
      // controls sit below it. Fixed elements ignore the body's inset padding.
      'fixed inset-x-0 top-0 z-20 flex items-center gap-2 px-3',
      'h-[calc(3.5rem+env(safe-area-inset-top))] pt-[env(safe-area-inset-top)]',
      'bg-gradient-to-b from-background/95 to-transparent transition-opacity duration-300',
      visible ? 'opacity-100' : 'pointer-events-none opacity-0',
    )}>
      <Button data-testid="menu" variant="ghost" size="icon" onClick={onMenu} title="Library (Esc closes)"
              className="size-9 rounded-full text-muted-foreground hover:text-foreground">
        <Menu className="size-4" />
      </Button>
      <Button data-testid="play" variant="ghost" size="icon" disabled={disabled} onClick={n.toggle}
              title="Play / pause (Space)"
              className="size-9 rounded-full text-muted-foreground hover:text-foreground">
        {n.playing ? <Pause className="size-4" /> : <Play className="size-4" />}
      </Button>
      <VoiceNote disabled={disabled} />

      {/* What survived the "read with your eyes" toggle: the text size it never
          owned but was sitting next to. It lives here and in the drawer. */}
      <div className="hidden items-center gap-2 text-muted-foreground sm:flex" title="Reading text size">
        <Type className="size-3" />
        <Slider className="w-24" min={70} max={180} step={5}
                value={[Math.round(n.fontScale * 100)]}
                onValueChange={([v]) => n.setFontScale(v / 100)}
                aria-label="Reading text size" />
      </div>

      <Diagnostics />
      <Connection />
      <div className="shrink-0 text-[11px] tracking-wide text-muted-foreground">
        {n.chunks.length ? <><b className="font-normal text-foreground/70">{n.idx + 1}</b>/{n.chunks.length}</> : null}
      </div>
    </div>
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

function Diagnostics() {
  const n = useNarrator();
  const s = n.status;
  let text = '';
  if (n.message) text = n.message;
  else if (n.conn === 'offline')
    text = n.mode === 'chunk' ? 'offline · reading from this device' : 'offline · audio from this device';
  else if (n.chapterLoading) text = 'loading the chapter…';
  else if (s) {
    const bits: string[] = [];
    if (n.textBusy && n.textProgress)
      bits.push(`saving text ${n.textProgress.done}/${n.textProgress.total}`);
    if (!s.model_ready) bits.push('loading model');
    if (n.mode === 'hls') bits.push('streaming');
    else if (n.mode === 'chapter') bits.push('chapter audio');
    else {
      bits.push(n.waiting ? 'waiting for audio…' : s.status);
      bits.push(`buffer ${Math.max(0, s.render_idx - n.idx - 1)}`);
    }
    if (s.building != null) bits.push(`packing ch ${s.building + 1}`);
    if (s.rtf) bits.push(`RTF ${s.rtf}×`);
    if (s.done_min != null && s.book_min) bits.push(`${(s.done_min / 60).toFixed(1)}h cached`);
    if (s.disk_gb != null) bits.push(`${s.disk_gb}/${s.disk_cap_gb} GB`);
    text = bits.join('  ·  ');
  }
  return (
    <div data-testid="diag" className={cn('min-w-0 flex-1 truncate text-[10.5px] tracking-wide',
                       n.message ? 'text-destructive' : 'text-muted-foreground/70')}>
      {text}
    </div>
  );
}

export function BottomBar({visible}: {visible: boolean}) {
  const n = useNarrator();
  const pct = n.chunks.length > 1 ? (n.idx / (n.chunks.length - 1)) * 100 : 0;
  return (
    <div className={cn(
      // max(), not 1rem + inset: on a phone the additive form is 16px on top of
      // ~34px of home-indicator inset, which reads as an empty band under the
      // bar. The inset already is the breathing room; 1rem is the floor for
      // everything without one. App.tsx's reading area matches this height.
      'fixed inset-x-0 bottom-0 z-20 px-4 pb-[max(1rem,env(safe-area-inset-bottom))]',
      'bg-gradient-to-t from-background/95 to-transparent transition-opacity duration-300',
      visible ? 'opacity-100' : 'pointer-events-none opacity-0',
    )}>
      <Slider
        className="mb-3" min={0} max={Math.max(0, n.chunks.length - 1)} step={1}
        value={[n.idx]}
        onValueChange={([v]) => n.setIdx(v)}
        disabled={!n.chunks.length}
        aria-label="Position in the chapter"
      />
      <div className="flex items-center gap-2">
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
    </div>
  );
}

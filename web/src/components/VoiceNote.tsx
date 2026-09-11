/**
 * The mic button.
 *
 * Recording pauses playback - narration in the microphone ruins the transcript -
 * and resumes after. The blob goes straight into the outbox; it is never uploaded
 * from here, so a failed upload cannot lose a thought. Recording works in read
 * mode too, where nothing is playing at all: the note still carries the chapter
 * and chunk the eyes were on, which is what makes the fleeting note's quote and
 * deep link land on the right passage.
 */
import {useRef, useState} from 'react';
import {Circle} from 'lucide-react';
import {Button} from '@/components/ui/button';
import {cn} from '@/lib/utils';
import {useNarrator} from '@/state';

export function VoiceNote({disabled}: {disabled: boolean}) {
  const n = useNarrator();
  const [recording, setRecording] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const rec = useRef<MediaRecorder | null>(null);

  async function toggle() {
    if (rec.current) { rec.current.stop(); return; }
    let stream: MediaStream;
    try { stream = await navigator.mediaDevices.getUserMedia({audio: true}); }
    catch (e) {
      setError(`mic blocked: ${e instanceof Error ? e.name : e} (needs localhost or https)`);
      return;
    }
    setError(null);
    const resume = n.playing;
    if (n.playing) n.toggle();
    const mime = MediaRecorder.isTypeSupported('audio/webm;codecs=opus')
      ? 'audio/webm;codecs=opus' : '';
    const r = new MediaRecorder(stream, mime ? {mimeType: mime} : {});
    const parts: Blob[] = [];
    r.ondataavailable = (e) => { if (e.data.size) parts.push(e.data); };
    r.onstop = () => {
      stream.getTracks().forEach((t) => t.stop());
      rec.current = null;
      setRecording(false);
      const blob = new Blob(parts, {type: r.mimeType || 'audio/webm'});
      void n.queueNote(blob);
      if (resume) n.toggle();
    };
    rec.current = r;
    r.start();
    setRecording(true);
  }

  return (
    <Button
      variant="ghost" size="icon" disabled={disabled} onClick={() => void toggle()}
      title={error ?? 'Voice note (N) — records, transcribes, files a fleeting note at this position'}
      className={cn('size-9 rounded-full text-muted-foreground hover:text-foreground',
                    recording && 'animate-pulse text-destructive')}
    >
      <Circle className={cn('size-3.5', recording && 'fill-current')} />
    </Button>
  );
}

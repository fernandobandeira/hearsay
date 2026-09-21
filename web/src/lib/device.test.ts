import {describe, expect, test} from 'vitest';
import {
  DEVICE_HEADER, DEVICE_KEY, DEVICE_NAME_HEADER,
  deviceId, deviceLabel, labelFor, resolveDeviceId, type Store,
} from './device';

/** A `localStorage` that works, so the round trip can be asserted. */
const memStore = (seed: Record<string, string> = {}): Store & {seen: Record<string, string>} => {
  const seen = {...seed};
  return {
    seen,
    getItem: (k) => (k in seen ? seen[k] : null),
    setItem: (k, v) => { seen[k] = v; },
  };
};

/** A `localStorage` behind blocked site data: every access throws. */
const angryStore = (): Store => ({
  getItem() { throw new DOMException('The operation is insecure.', 'SecurityError'); },
  setItem() { throw new DOMException('The operation is insecure.', 'SecurityError'); },
});

describe('resolveDeviceId - an id, whatever the storage does', () => {
  test('an empty store is minted into and written back', () => {
    const s = memStore();
    const id = resolveDeviceId(s, () => 'minted-1');
    expect(id).toBe('minted-1');
    expect(s.seen[DEVICE_KEY]).toBe('minted-1');
  });

  test('an id already held is kept - this is what survives a page load', () => {
    const s = memStore({[DEVICE_KEY]: 'held-1'});
    expect(resolveDeviceId(s, () => 'minted-1')).toBe('held-1');
    expect(s.seen[DEVICE_KEY]).toBe('held-1');
  });

  /* The failure this prevents: on iOS with site data blocked, `localStorage`
     throws on access rather than answering null. Thrown from here it would come
     out of the first call that identifies the device - which is on the path of
     every position report - and the reader would lose position syncing entirely
     on exactly the browsers it is most often opened in. */
  test('a store that throws still yields a usable id', () => {
    expect(resolveDeviceId(angryStore(), () => 'minted-1')).toBe('minted-1');
  });

  test('no store at all is not an error either', () => {
    expect(resolveDeviceId(null, () => 'minted-1')).toBe('minted-1');
  });

  test('an empty string held is not an id', () => {
    const s = memStore({[DEVICE_KEY]: ''});
    expect(resolveDeviceId(s, () => 'minted-1')).toBe('minted-1');
  });

  test('the default mint is a fresh id every time', () => {
    const a = resolveDeviceId(null);
    const b = resolveDeviceId(null);
    expect(a).not.toBe(b);
    expect(a.length).toBeGreaterThan(8);
  });
});

describe('deviceId - stable for the life of the page', () => {
  /* The whole point: the id is compared for equality against itself, so a value
     that changed between two calls would make this device stop recognising its
     own echo halfway through a session. Under vitest there is no localStorage at
     all, which is also the in-memory fallback path. */
  test('every call returns the same id', () => {
    const first = deviceId();
    expect(deviceId()).toBe(first);
    expect(deviceId()).toBe(first);
    expect(first).not.toBe('');
  });
});

describe('labelFor - a guess, for one line of one screen', () => {
  test('the platforms Fernando actually reads on', () => {
    expect(labelFor('Mozilla/5.0 (iPhone; CPU iPhone OS 17_5 like Mac OS X) AppleWebKit/605.1.15'))
      .toBe('iPhone');
    expect(labelFor('Mozilla/5.0 (iPad; CPU OS 17_5 like Mac OS X) AppleWebKit/605.1.15'))
      .toBe('iPad');
    expect(labelFor('Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36'))
      .toBe('Mac');
    expect(labelFor('Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36')).toBe('Windows');
    expect(labelFor('Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36')).toBe('Linux');
  });

  /* Every Android user agent says Linux, so the order of the tests is the whole
     of the correctness here. An iPhone's says "like Mac OS X" for the same
     reason in the other direction. */
  test('Android is not Linux, and an iPhone is not a Mac', () => {
    expect(labelFor('Mozilla/5.0 (Linux; Android 14; Pixel 8) AppleWebKit/537.36')).toBe('Android');
    expect(labelFor('Mozilla/5.0 (iPhone; CPU iPhone OS 17_5 like Mac OS X)')).toBe('iPhone');
  });

  test('userAgentData wins where it is offered', () => {
    expect(labelFor('', 'Windows')).toBe('Windows');
    expect(labelFor('', 'macOS')).toBe('Mac');
  });

  test('nothing recognisable is "device", not an empty label', () => {
    expect(labelFor('')).toBe('device');
    expect(labelFor('Node.js/24')).toBe('device');
  });
});

describe('deviceLabel - asking this browser', () => {
  test('it answers something, whatever is running the tests', () => {
    expect(typeof deviceLabel()).toBe('string');
    expect(deviceLabel().length).toBeGreaterThan(0);
  });
});

describe('the headers', () => {
  /* Spelled out because the server matches on them and the two halves are in
     different languages: a typo here is a device id that silently never arrives,
     and the symptom is the echo suppression quietly going back to guessing. */
  test('are the names the server reads', () => {
    expect(DEVICE_HEADER).toBe('X-Narrator-Device');
    expect(DEVICE_NAME_HEADER).toBe('X-Narrator-Device-Name');
  });
});

/* Narrator for Obsidian - the HearSay reader, framed.
 *
 * This used to be a reader of its own, a hand port of the python server's UI,
 * which meant every change to the real reader (web/ in this repo) had to be
 * ported again or Obsidian fell behind. Now the pane is an iframe onto the
 * server's own reader, so it is whatever the box is serving, and all that is
 * left here is what only Obsidian can do:
 *
 *  - .epub files in the vault open in the pane, at that book
 *  - obsidian://narrator-open?book=..&chapter=..&chunk=..  - the link every
 *    voice note carries back to its passage
 *  - "Insert reading position link", which asks the reader where it is
 *
 * The protocol between the two is web/src/lib/embed.ts: `?open=` on the frame's
 * src for the first open, postMessage after the reader says `hearsay:ready`.
 *
 * Hand-written CommonJS on purpose: no dependencies, no build step. It is
 * copied into the vault's .obsidian/plugins/narrator/ - never symlinked, or a
 * second machine cloning the vault gets a link to a folder it does not have.
 * The server URL lives in the vault's data.json, not here: this repo is public.
 */
const { Plugin, FileView, Notice, PluginSettingTab, Setting } = require("obsidian");

const VIEW_TYPE = "narrator-reader";
const DEFAULTS = { server: "" };

class NarratorView extends FileView {
  constructor(leaf, plugin) {
    super(leaf);
    this.allowNoFile = true;          // the ribbon opens it with no book
    this.plugin = plugin;
    this.frame = null;
    this.ready = false;
    this.outbox = [];                 // messages waiting for hearsay:ready
    this.waiters = new Map();         // where-request id -> resolve
    this.onMessage = this.onMessage.bind(this);
  }

  getViewType() { return VIEW_TYPE; }
  getDisplayText() { return this.file ? this.file.basename : "Narrator"; }
  getIcon() { return "book-audio"; }
  canAcceptExtension(ext) { return ext === "epub"; }

  origin() {
    try { return new URL(this.plugin.settings.server).origin; } catch (_) { return null; }
  }

  async onOpen() {
    this.contentEl.addClass("narrator-view");
    window.addEventListener("message", this.onMessage);
    this.mount();
  }

  async onClose() {
    window.removeEventListener("message", this.onMessage);
    for (const r of this.waiters.values()) r(null);
    this.waiters.clear();
  }

  /* (Re)build the frame. `open` rides on the src, so the first book needs no
     handshake at all. */
  mount(open) {
    const c = this.contentEl;
    c.empty();
    this.ready = false;
    this.frame = null;
    if (!this.origin()) {
      c.createDiv({ cls: "narrator-empty",
                    text: "Set the HearSay server URL in Settings → Narrator." });
      return;
    }
    const u = new URL(this.plugin.settings.server);
    if (open) {
      u.searchParams.set("open", open.book);
      if (open.chapter != null) u.searchParams.set("chapter", String(open.chapter));
      if (open.chunk) u.searchParams.set("chunk", String(open.chunk));
    }
    this.frame = c.createEl("iframe", { attr: {
      src: u.toString(),
      allow: "microphone; autoplay; clipboard-write; fullscreen",
    } });
  }

  onMessage(e) {
    if (!this.frame || e.source !== this.frame.contentWindow) return;
    if (e.origin !== this.origin()) return;
    const m = e.data;
    if (!m || typeof m !== "object") return;
    if (m.type === "hearsay:ready") {
      this.ready = true;
      for (const x of this.outbox.splice(0)) this.send(x);
    } else if (m.type === "hearsay:here" && this.waiters.has(m.id)) {
      this.waiters.get(m.id)(m);
      this.waiters.delete(m.id);
    }
  }

  send(m) {
    if (!this.frame) return;
    if (!this.ready) { this.outbox.push(m); return; }
    this.frame.contentWindow.postMessage(m, this.origin());
  }

  /* Open a book at a position: through the live reader if there is one, or as
     the frame's first page if there is not. */
  openBook(book, chapter, chunk) {
    const open = { book, chapter: chapter == null ? null : chapter, chunk: chunk || 0 };
    if (this.frame) this.send({ type: "hearsay:open", ...open });
    else this.mount(open);
  }

  where() {
    if (!this.frame) return Promise.resolve(null);
    const id = Math.random().toString(36).slice(2);
    return new Promise((resolve) => {
      this.waiters.set(id, resolve);
      this.send({ type: "hearsay:where", id });
      setTimeout(() => { if (this.waiters.delete(id)) resolve(null); }, 3000);
    });
  }

  async onLoadFile(file) { this.openBook(file.name); }
  async onUnloadFile() {}
}

class NarratorSettings extends PluginSettingTab {
  constructor(app, plugin) { super(app, plugin); this.plugin = plugin; }
  display() {
    const { containerEl } = this;
    containerEl.empty();
    new Setting(containerEl)
      .setName("HearSay server URL")
      .setDesc("Where the reader is served, e.g. the box's https address on the tailnet. Reopen the pane after changing it.")
      .addText((t) => t.setPlaceholder("https://…").setValue(this.plugin.settings.server)
        .onChange(async (v) => { this.plugin.settings.server = v.trim().replace(/\/$/, "");
                                 await this.plugin.saveSettings(); }));
  }
}

module.exports = class NarratorPlugin extends Plugin {
  async onload() {
    this.settings = Object.assign({}, DEFAULTS, await this.loadData());
    this.registerView(VIEW_TYPE, (leaf) => new NarratorView(leaf, this));
    try { this.registerExtensions(["epub"], VIEW_TYPE); }
    catch (_) { new Notice("Narrator: another plugin already handles .epub files"); }
    this.addRibbonIcon("book-audio", "Narrator", () => this.openView());
    this.addSettingTab(new NarratorSettings(this.app, this));

    this.addCommand({ id: "open-reader", name: "Open reader", callback: () => this.openView() });
    this.addCommand({
      id: "insert-position", name: "Insert current reading position link",
      editorCallback: async (editor) => {
        const v = this.reader();
        const h = v ? await v.where() : null;
        if (!h || !h.book) { new Notice("No book open in the narrator reader."); return; }
        const url = "obsidian://narrator-open?book=" + encodeURIComponent(h.book)
          + "&chapter=" + h.chapter + "&chunk=" + h.chunk;
        const label = h.book.replace(/\.epub$/i, "") + " — " + h.chapterTitle + ", ¶" + (h.chunk + 1);
        editor.replaceSelection("[📖 " + label + "](" + url + ")");
      },
    });

    /* obsidian://narrator-open?book=<file.epub>&chapter=N&chunk=M */
    this.registerObsidianProtocolHandler("narrator-open", async (p) => {
      if (!p.book) return;
      const v = await this.openView();
      const n = (s) => { const x = parseInt(s, 10); return Number.isInteger(x) && x >= 0 ? x : null; };
      v.openBook(p.book, n(p.chapter), n(p.chunk) || 0);
    });
  }

  reader() {
    const leaf = this.app.workspace.getLeavesOfType(VIEW_TYPE)[0];
    return leaf ? leaf.view : null;
  }

  async openView() {
    let leaf = this.app.workspace.getLeavesOfType(VIEW_TYPE)[0];
    if (!leaf) {
      leaf = this.app.workspace.getLeaf("split");
      await leaf.setViewState({ type: VIEW_TYPE, active: true });
    }
    this.app.workspace.revealLeaf(leaf);
    return leaf.view;
  }

  async saveSettings() { await this.saveData(this.settings); }
};

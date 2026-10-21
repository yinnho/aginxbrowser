// <details> / <summary> (split from bootstrap.js): the `open`/`name`
// reflection, the async coalesced `toggle` event, name-group exclusivity, and
// the summary's activation behaviour (a click on the first <summary> child —
// or non-interactive content inside it — flips `open`). Everything is
// call-time.
(() => {
  const D = HTMLDetailsElement.prototype;
  const pending = new WeakMap();
  const notify = (el, wasOpen) => {
    if (pending.has(el)) return;
    pending.set(el, wasOpen ? 'open' : 'closed');
    setTimeout(() => {
      const oldState = pending.get(el);
      pending.delete(el);
      const newState = el.hasAttribute('open') ? 'open' : 'closed';
      try { el.dispatchEvent(new ToggleEvent('toggle', { oldState, newState })); } catch (e) {}
    }, 0);
  };
  // setAttribute/removeAttribute('open') call this BEFORE the write, so the old
  // state is still readable; the timer compares it with whatever the attribute
  // is by then (a flip-flop within one turn still fires one event, closed>closed, as in Chrome).
  globalThis.__detailsAttr = (el) => { if (el.localName === 'details') notify(el, el.hasAttribute('open')); };
  Object.defineProperty(D, 'open', {
    enumerable: true, configurable: true,
    get() { return this.hasAttribute('open'); },
    set(v) {
      const was = this.hasAttribute('open');
      if (!!v === was) return;
      if (v) {
        this.setAttribute('open', '');
        // Name group (exclusive accordion): opening one closes the open others
        // that share its name in the same tree.
        const name = this.getAttribute('name');
        if (name) {
          const root = this.getRootNode();
          if (root && root.querySelectorAll) {
            for (const o of root.querySelectorAll('details[name]')) {
              if (o !== this && o.getAttribute('name') === name && o.hasAttribute('open')) o.open = false;
            }
          }
        }
      } else {
        this.removeAttribute('open');
      }
      notify(this, was);
    },
  });
  Object.defineProperty(D, 'name', {
    enumerable: true, configurable: true,
    get() { return this.getAttribute('name') ?? ''; },
    set(v) { this.setAttribute('name', String(v)); },
  });
  const INTERACTIVE = 'a[href],button,input,select,textarea,label,summary';
  // Called from _postClickActivation; true when the click toggled a <details>.
  globalThis.__summaryActivation = function (el) {
    let n = el;
    while (n && n.localName !== 'summary') {
      if (n.nodeType === 1 && n.matches && n.matches(INTERACTIVE)) return false;
      n = n.parentNode;
    }
    if (!n) return false;
    const details = n.parentNode;
    if (!details || details.localName !== 'details') return false;
    for (const c of details.children) {
      if (c.localName === 'summary') {
        if (c !== n) return false;
        break;
      }
    }
    details.open = !details.open;
    return true;
  };
})();

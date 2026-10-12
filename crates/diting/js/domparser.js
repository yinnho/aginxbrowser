// (#97 extraction split) DOMParser, moved out of bootstrap.js (god-file
// ratchet: the file only shrinks). Runs after bootstrap.js in the same
// realm — the XML gates (_checkXmlWellFormed/_xmlWellFormed/_xmlBuildTree),
// _getElementsByClassName, Range/CDATASection/ProcessingInstruction and
// _dom are bootstrap top-level bindings shared across scripts, exactly as
// they were in-file.

// Real-enough DOMParser. The previous one-liner returned `globalThis.document`,
// so anything that did `new DOMParser().parseFromString(s, 'text/html')` and
// then read `.body.innerHTML` mutated the LIVE page (jQuery 3.x's selector
// feature-detect writes `<form></form>` and wiped real bodies). We parse the
// input into a detached `<html>` element and wrap it so the common Document
// API surface (body / head / documentElement / querySelector* / getElementById /
// getElementsByTagName / getElementsByClassName / title / cloneNode) works.
globalThis.DOMParser = class DOMParser {
  parseFromString(source, mimeType) {
    const html = String(source ?? "");
    const isXml = typeof mimeType === "string" && /xml/i.test(mimeType);

    // XML mime types: two well-formedness gates first (conservative
    // regex pass with specific error text, then the stricter state
    // machine). Valid input goes to the hand-rolled XML tree builder so
    // the root element keeps its real name, case, namespace, and
    // self-closing structure (obscura#883) — the HTML fragment parser
    // collapsed all of that into an <html> root.
    const xmlError = isXml ? _checkXmlWellFormed(html) : null;
    const strictBad = isXml && !_xmlWellFormed(html);
    let root = document.createElement("html");
    let built = null;
    if (isXml && !(xmlError && !xmlError.wellFormed) && !strictBad) {
      built = _xmlBuildTree(html);
    }
    if (built) {
      root = built;
    } else if (xmlError && !xmlError.wellFormed) {
      // Build the <parsererror> element directly rather than via innerHTML:
      // the fragment parser routes unknown elements into <head>, which would
      // make firstElementChild (the documentElement) a <head> instead of the
      // parsererror Chrome would hand back.
      const pe = document.createElement('parsererror');
      pe.textContent = xmlError.error;
      root.appendChild(pe);
    } else if (isXml) {
      // Strict-gate failure or builder bail on clearly-malformed input:
      // generic <parsererror> so error-detection code
      // (doc.querySelector('parsererror')) works, matching Chrome.
      try {
        root.innerHTML = '';
        const pe = document.createElement('parsererror');
        pe.setAttribute('xmlns', 'http://www.w3.org/1999/xhtml');
        pe.innerHTML = 'This page contains the following errors:<div>error while parsing XML</div>';
        root.appendChild(pe);
      } catch (e) { /* ignore */ }
    } else {
      // innerHTML parses children via html5ever fragment-parsing rules. Most
      // HTML inputs start with `<!DOCTYPE>` / `<html>` / `<head>` etc.; the
      // fragment parser strips the outer `<html>` and emits its head+body
      // children, which is what callers want.
      try { root.innerHTML = html; } catch (e) { /* leave empty on parse error */ }
    }
    // All XML failure paths leave `root` as the <html> wrapper with a
    // <parsererror> child; expose that child as documentElement like Chrome.
    const isParserError = !built && isXml;

    // Helper: depth-first walk to find an element by predicate.
    const walk = (node, pred) => {
      if (!node) return null;
      if (node.nodeType === 1 && pred(node)) return node;
      const children = node.children || [];
      for (let i = 0; i < children.length; i++) {
        const r = walk(children[i], pred);
        if (r) return r;
      }
      return null;
    };

    const findByTagName = (name) => walk(root, n => n.tagName === name);

    const docNode = {
      _root: root,
      nodeName: "#document",
      nodeType: 9,
      contentType: isXml ? (mimeType || "application/xml") : "text/html",
      get documentElement() {
        // For XML parsererror docs, return the <parsererror> child, not the
        // <html> wrapper — matches Chrome's behavior.
        if (isParserError) return root.firstElementChild;
        return root;
      },
      get body() { return findByTagName("BODY"); },
      get head() { return findByTagName("HEAD"); },
      get title() {
        const t = findByTagName("TITLE");
        return t ? (t.textContent || "").replace(/[\t\n\f\r ]+/g, " ").trim() : "";
      },
      set title(value) {
        let t = findByTagName("TITLE");
        if (!t) {
          let head = findByTagName("HEAD");
          if (!head) {
            head = document.createElement("head");
            root.insertBefore(head, findByTagName("BODY"));
          }
          t = document.createElement("title");
          head.appendChild(t);
        }
        t.textContent = String(value);
      },
      get referrer() { return ""; },
      get referrerPolicy() { return ""; },
      get firstChild() { return root; },
      get lastChild() { return root; },
      get children() { return [root]; },
      get childNodes() { return [root]; },
      // Document metadata the WHATWG interface exposes; DOMParser documents have
      // URL about:blank, are already fully parsed, and carry no stylesheets.
      get URL() { return "about:blank"; },
      get documentURI() { return "about:blank"; },
      get baseURI() { return "about:blank"; },
      get compatMode() { return "CSS1Compat"; },
      get characterSet() { return "UTF-8"; },
      get charset() { return "UTF-8"; },
      get inputEncoding() { return "UTF-8"; },
      get readyState() { return "complete"; },
      get styleSheets() { return { length: 0, item() { return null; }, [Symbol.iterator]: function* () {} }; },
      get defaultView() { return null; },
      get ownerDocument() { return null; },
      createTreeWalker(r, ws, f) { return document.createTreeWalker(r || root, ws, f); },
      createNodeIterator(r, ws, f) { return document.createNodeIterator(r || root, ws, f); },
      querySelector(s) {
        // For XML parsererror docs, check the root element as well —
        // the <parsererror> is the documentElement, not a descendant.
        return root.querySelector(s) || (isParserError && root.matches(s) ? root : null);
      },
      querySelectorAll(s) { return root.querySelectorAll(s); },
      getElementById(id) {
        const needle = String(id);
        return needle === "" ? null : walk(root, n => n.getAttribute && n.getAttribute("id") === needle);
      },
      getElementsByTagName(t) {
        return root.querySelectorAll(t);
      },
      getElementsByClassName(c) {
        return _getElementsByClassName(root, c);
      },
      getElementsByName(n) {
        return root.querySelectorAll(`[name="${n}"]`);
      },
      // Spec collections — same family as the live Document (XHS
      // 2026-10-12: the SDK read document.scripts on a DOMParser doc and
      // got undefined where Chrome hands an HTMLCollection). Anchors/embeds
      // stay unlisted, matching the live Document's exposed set exactly.
      get scripts() { return HTMLCollection._from(this.querySelectorAll('script')); },
      get forms() { return HTMLCollection._from(this.querySelectorAll('form')); },
      get images() { return HTMLCollection._from(this.querySelectorAll('img')); },
      get links() { return HTMLCollection._from(this.querySelectorAll('a[href], area[href]')); },
      // No script of this document is executing — Chrome reports null here.
      get currentScript() { return null; },
      createElement: (t) => document.createElement(t),
      createElementNS: (ns, t) => document.createElement(t),
      createTextNode: (t) => document.createTextNode(t),
      createComment: (t) => document.createComment(t),
      createDocumentFragment: () => document.createDocumentFragment(),
      createRange: () => new Range(),
      createEvent: (type) => document.createEvent(type),
      createCDATASection: (data) => {
        if (mimeType === "text/html") throw new DOMException("createCDATASection is not supported in HTML documents", "NotSupportedError");
        const s = String(data);
        if (s.indexOf("]]>") !== -1) throw new DOMException("CDATA section data must not contain ']]>'", "InvalidCharacterError");
        return new CDATASection(+_dom("create_text_node", s));
      },
      createProcessingInstruction: (target, data) => {
        const t = String(target), s = String(data);
        if (!_isValidPITarget(t)) throw new DOMException("Invalid processing instruction target", "InvalidCharacterError");
        if (s.indexOf("?>") !== -1) throw new DOMException("Processing instruction data must not contain '?>'", "InvalidCharacterError");
        return new ProcessingInstruction(+_dom("create_text_node", s), t);
      },
      adoptNode: (n) => n,
      importNode: (n) => n,
      // Document-level node insertion. Detached docs from createHTMLDocument /
      // createDocument back onto the same tree, so appending lands under the
      // documentElement; enough for dom/common.js to build its Range fixtures.
      appendChild: function (n) { try { root.appendChild(n); } catch (e) {} return n; },
      removeChild: function (n) { try { root.removeChild(n); } catch (e) {} return n; },
      insertBefore: function (n, ref) { try { root.insertBefore(n, ref); } catch (e) {} return n; },
      _docType: null,
      get doctype() { return this._docType; },
      cloneNode: function (deep) {
        return new DOMParser().parseFromString(root.outerHTML, mimeType);
      },
      contains(n) { return root.contains ? root.contains(n) : false; },
      addEventListener() {}, removeEventListener() {}, dispatchEvent() { return true; },
    };
    return docNode;
  }
};

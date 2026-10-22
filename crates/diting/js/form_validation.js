// Constraint validation API (split from bootstrap.js): live ValidityState,
// validationMessage (Chrome's en-US texts), willValidate, checkValidity /
// reportValidity / setCustomValidity on controls, form.checkValidity /
// reportValidity / noValidate, and the interactive validation requestSubmit
// runs before a submit event. Everything is computed on read from the
// control's attributes and value; call-time only.
(() => {
  const attr = (el, n) => el.getAttribute(n);
  const has = (el, n) => el.hasAttribute(n);
  const typeOf = (el) => {
    if (el.localName !== 'input') return el.localName;
    const t = (attr(el, 'type') || 'text').toLowerCase();
    return ['hidden','text','search','tel','url','email','password','date','month','week','time','datetime-local','number','range','color','checkbox','radio','file','submit','image','reset','button'].includes(t) ? t : 'text';
  };
  const custom = new WeakMap();
  const fieldsetDisabled = (el) => {
    for (let p = el.parentNode; p; p = p.parentNode) {
      if (p.localName === 'fieldset' && has(p, 'disabled')) {
        const legend = Array.from(p.children).find((c) => c.localName === 'legend');
        if (!(legend && legend.contains(el))) return true;
      }
    }
    return false;
  };
  const TEXTLIKE = new Set(['text','search','tel','url','email','password']);
  const READONLY_APPLIES = new Set(['text','search','tel','url','email','password','date','month','week','time','datetime-local','number']);
  const willValidate = (el) => {
    const n = el.localName;
    if (!['input','select','textarea','button','output','object','fieldset'].includes(n)) return false;
    if (n === 'fieldset' || n === 'output' || n === 'object') return false;
    if (has(el, 'disabled') || fieldsetDisabled(el)) return false;
    for (let p = el.parentNode; p; p = p.parentNode) if (p.localName === 'datalist') return false;
    if (n === 'button') return (attr(el, 'type') || 'submit').toLowerCase() === 'submit';
    if (n === 'input') {
      const t = typeOf(el);
      if (['hidden','reset','button'].includes(t)) return false;
      if (has(el, 'readonly') && READONLY_APPLIES.has(t)) return false;
    }
    if (n === 'textarea' && has(el, 'readonly')) return false;
    return true;
  };
  const num = (s) => { const n = Number(s); return s !== '' && s != null && Number.isFinite(n) ? n : NaN; };
  // date-ish types -> a comparable number, NaN when malformed.
  const temporal = (t, s) => {
    let m;
    switch (t) {
      case 'date': m = /^(\d{4,})-(\d{2})-(\d{2})$/.exec(s); return m ? Date.UTC(+m[1], +m[2] - 1, +m[3]) / 864e5 : NaN;
      case 'month': m = /^(\d{4,})-(\d{2})$/.exec(s); return m ? (+m[1]) * 12 + (+m[2] - 1) : NaN;
      case 'week': m = /^(\d{4,})-W(\d{2})$/.exec(s); return m ? (+m[1]) * 53 + (+m[2]) : NaN;
      case 'time': m = /^(\d{2}):(\d{2})(?::(\d{2})(?:\.(\d{1,3}))?)?$/.exec(s); return m ? (+m[1]) * 3600 + (+m[2]) * 60 + (+(m[3] || 0)) + (+(m[4] || 0)) / 1000 : NaN;
      case 'datetime-local': m = /^(\d{4,})-(\d{2})-(\d{2})T(\d{2}):(\d{2})(?::(\d{2}))?$/.exec(s); return m ? Date.UTC(+m[1], +m[2] - 1, +m[3], +m[4], +m[5], +(m[6] || 0)) / 1000 : NaN;
      default: return NaN;
    }
  };
  const parse = (t, s) => (t === 'number' || t === 'range') ? num(s) : temporal(t, s);
  const RANGE_TYPES = new Set(['number','range','date','month','week','time','datetime-local']);
  const EMAIL = /^[a-zA-Z0-9.!#$%&'*+/=?^_`{|}~-]+@[a-zA-Z0-9](?:[a-zA-Z0-9-]{0,61}[a-zA-Z0-9])?(?:\.[a-zA-Z0-9](?:[a-zA-Z0-9-]{0,61}[a-zA-Z0-9])?)*$/;
  const groupMembers = (el) => {
    const name = attr(el, 'name');
    if (!name) return [el];
    const root = el.form || el.getRootNode();
    return Array.from(root.querySelectorAll('input[type=radio]')).filter((r) => attr(r, 'name') === name && r.form === el.form);
  };
  const groupChecked = (el) => groupMembers(el).some((r) => r.checked);
  const stepInfo = (el, t) => {
    const raw = attr(el, 'step');
    if (raw && raw.toLowerCase() === 'any') return null;
    const dflt = { number: 1, range: 1, date: 1, month: 1, week: 1, time: 60, 'datetime-local': 60 }[t];
    const mult = { date: 1, month: 1, week: 1, time: 1, 'datetime-local': 1 }[t] || 1;
    let step = raw != null ? num(raw) : NaN;
    if (!(step > 0)) step = dflt; else step *= mult;
    const min = parse(t, attr(el, 'min') || '');
    const base = Number.isFinite(min) ? min : (Number.isFinite(parse(t, attr(el, 'value') || '')) ? parse(t, attr(el, 'value')) : (t === 'week' ? temporal('week', '1970-W01') : 0));
    return { step: t === 'number' || t === 'range' ? step : step, base };
  };
  const state = (el) => {
    const out = { valueMissing: false, typeMismatch: false, patternMismatch: false, tooLong: false, tooShort: false, rangeUnderflow: false, rangeOverflow: false, stepMismatch: false, badInput: false, customError: false };
    out.customError = !!custom.get(el);
    if (!willValidate(el)) return out;
    const n = el.localName;
    if (n === 'select') {
      if (has(el, 'required')) {
        const opts = Array.from(el.options || []);
        const sel = opts.filter((o) => o.selected);
        const placeholder = !has(el, 'multiple') && !(Number(attr(el, 'size')) > 1);
        out.valueMissing = sel.length === 0 || sel.every((o) => o.value === '') && (placeholder ? (sel[0] === opts[0]) : true);
      }
      return out;
    }
    if (n === 'textarea') {
      out.valueMissing = has(el, 'required') && el.value === '';
      return out;
    }
    if (n !== 'input') return out;
    const t = typeOf(el);
    const v = el.value == null ? '' : String(el.value);
    // A radio group is required when any of its members is.
    const groupRequired = t === 'radio' && (has(el, 'required') || groupMembers(el).some((r) => has(r, 'required')));
    if (has(el, 'required') || groupRequired) {
      if (t === 'checkbox') out.valueMissing = !el.checked;
      else if (t === 'radio') out.valueMissing = !groupChecked(el);
      else if (t === 'file') out.valueMissing = !(el.files && el.files.length);
      else if (t !== 'range' && t !== 'color') out.valueMissing = v === '';
    }
    if (v !== '') {
      if (t === 'email') {
        const parts = has(el, 'multiple') ? v.split(',').map((s) => s.trim()) : [v.trim()];
        out.typeMismatch = !parts.every((p) => EMAIL.test(p));
      } else if (t === 'url') {
        try { new URL(v); } catch (e) { out.typeMismatch = true; }
      }
      if (TEXTLIKE.has(t) && has(el, 'pattern')) {
        try { out.patternMismatch = !new RegExp('^(?:' + attr(el, 'pattern') + ')$', 'v').test(v); } catch (e) {}
      }
      if (RANGE_TYPES.has(t)) {
        const x = parse(t, v);
        if (Number.isFinite(x)) {
          const min = parse(t, attr(el, 'min') || ''), max = parse(t, attr(el, 'max') || '');
          if (Number.isFinite(min) && x < min) out.rangeUnderflow = true;
          if (Number.isFinite(max) && x > max) out.rangeOverflow = true;
          const si = stepInfo(el, t);
          if (si && (t === 'number' || t === 'range' || t === 'date')) {
            const q = (x - si.base) / si.step;
            out.stepMismatch = Math.abs(q - Math.round(q)) > 1e-9;
          }
        }
      }
    }
    return out;
  };
  const fmtNum = (x) => String(Math.round(x * 1e6) / 1e6);
  // Chrome words these in the browser UI language; the persona language
  // (navigator.language) stands in for it.
  const zh = () => /^zh/i.test(String(globalThis.navigator && navigator.language));
  const fmtDate = (t, s) => {
    const m = /^(\d{4,})-(\d{2})-(\d{2})/.exec(s || '');
    if (t === 'date' && m) return zh() ? `${m[1]}/${m[2]}/${m[3]}` : `${+m[2]}/${+m[3]}/${m[1]}`;
    return s;
  };
  const T = {
    missing: ['Please fill out this field.', '请填写此字段。'],
    checkbox: ['Please check this box if you want to proceed.', '如果要继续，请选中此框。'],
    radio: ['Please select one of these options.', '请从这些选项中选择一个。'],
    file: ['Please select a file.', '请选择一个文件。'],
    select: ['Please select an item in the list.', '请在列表中选择一项。'],
    email: ['Please enter an email address.', '请输入电子邮件地址。'],
    url: ['Please enter a URL.', '请输入网址。'],
    pattern: ['Please match the requested format.', '请与所请求的格式保持一致。'],
  };
  const t2 = (k) => T[k][zh() ? 1 : 0];
  const message = (el) => {
    const c = custom.get(el);
    if (c) return c;
    const f = state(el);
    const t = el.localName === 'input' ? typeOf(el) : el.localName;
    const Z = zh();
    if (f.valueMissing) return t2(['checkbox', 'radio', 'file', 'select'].includes(t) ? t : 'missing');
    if (f.typeMismatch) {
      const v = String(el.value);
      if (t === 'email') {
        if (!v.includes('@')) return Z ? `请在电子邮件地址中包括“@”。“${v}”中缺少“@”。` : `Please include an '@' in the email address. '${v}' is missing an '@'.`;
        const [local, domain] = v.split('@');
        if (!local) return Z ? `请在“@”前面输入内容。“${v}”不完整。` : `Please enter a part followed by '@'. '${v}' is incomplete.`;
        if (!domain) return Z ? `请在“@”后面输入内容。“${v}”不完整。` : `Please enter a part following '@'. '${v}' is incomplete.`;
        return t2('email');
      }
      return t2('url');
    }
    if (f.patternMismatch) return t2('pattern');
    const numeric = t === 'number' || t === 'range';
    if (f.rangeUnderflow) {
      const min = attr(el, 'min');
      if (numeric) return Z ? `值必须大于或等于 ${fmtNum(num(min))}。` : `Value must be greater than or equal to ${fmtNum(num(min))}.`;
      return Z ? `指定的值不得早于${fmtDate(t, min)}。` : `Value must be ${fmtDate(t, min)} or later.`;
    }
    if (f.rangeOverflow) {
      const max = attr(el, 'max');
      if (numeric) return Z ? `值必须小于或等于 ${fmtNum(num(max))}。` : `Value must be less than or equal to ${fmtNum(num(max))}.`;
      return Z ? `指定的值不得晚于${fmtDate(t, max)}。` : `Value must be ${fmtDate(t, max)} or earlier.`;
    }
    if (f.stepMismatch) {
      const si = stepInfo(el, t), x = num(el.value);
      const lo = si.base + Math.floor((x - si.base) / si.step) * si.step, hi = lo + si.step;
      return Z ? `请输入有效值。两个最接近的有效值分别为${fmtNum(lo)}和${fmtNum(hi)}。` : `Please enter a valid value. The two nearest valid values are ${fmtNum(lo)} and ${fmtNum(hi)}.`;
    }
    return '';
  };
  class ValidityState {
    constructor(el, key) { if (key !== KEY) throw new TypeError('Illegal constructor'); this._el = el; __hideOwn(this); }
    get valid() { const s = state(this._el); return !Object.values(s).some(Boolean); }
  }
  const KEY = {};
  for (const k of ['valueMissing','typeMismatch','patternMismatch','tooLong','tooShort','rangeUnderflow','rangeOverflow','stepMismatch','badInput','customError']) {
    Object.defineProperty(ValidityState.prototype, k, { enumerable: true, configurable: true, get() { return state(this._el)[k]; } });
  }
  Object.defineProperty(ValidityState.prototype, Symbol.toStringTag, { value: 'ValidityState', configurable: true });
  globalThis.ValidityState = ValidityState;
  _markNative(ValidityState);
  const views = new WeakMap();
  const invalid = (el) => willValidate(el) && Object.values(state(el)).some(Boolean);
  const fire = (el) => el.dispatchEvent(new Event('invalid', { cancelable: true }));
  const controlMethods = {
    validity: { get() { if (!views.has(this)) views.set(this, new ValidityState(this, KEY)); return views.get(this); } },
    validationMessage: { get() { return willValidate(this) ? message(this) : ''; } },
    willValidate: { get() { return willValidate(this); } },
  };
  const control = {
    checkValidity() { if (!invalid(this)) return true; fire(this); return false; },
    reportValidity() {
      if (!invalid(this)) return true;
      if (fire(this) && typeof this.focus === 'function') { try { this.focus(); } catch (e) {} }
      return false;
    },
    setCustomValidity(msg) { const s = String(msg); if (s) custom.set(this, s); else custom.delete(this); },
  };
  const FORM_TAGS = ['input', 'select', 'textarea', 'button'];
  const listed = (form) => Array.from(form.querySelectorAll(FORM_TAGS.join(',')));
  const install = (C, withAll) => {
    for (const [k, d] of Object.entries(controlMethods)) Object.defineProperty(C.prototype, k, { enumerable: true, configurable: true, ...d });
    for (const [k, f] of Object.entries(control)) { Object.defineProperty(C.prototype, k, { value: f, writable: true, configurable: true }); _markNative(C.prototype[k]); }
  };
  for (const C of [HTMLInputElement, HTMLSelectElement, HTMLTextAreaElement, HTMLButtonElement]) install(C);
  const formCheck = (form, report) => {
    const bad = listed(form).filter(invalid);
    let first = null;
    for (const el of bad) { const notCanceled = fire(el); if (notCanceled && !first) first = el; }
    if (report && first && typeof first.focus === 'function') { try { first.focus(); } catch (e) {} }
    return bad.length === 0;
  };
  Object.defineProperty(HTMLFormElement.prototype, 'checkValidity', { value() { return formCheck(this, false); }, writable: true, configurable: true });
  Object.defineProperty(HTMLFormElement.prototype, 'reportValidity', { value() { return formCheck(this, true); }, writable: true, configurable: true });
  _markNative(HTMLFormElement.prototype.checkValidity); _markNative(HTMLFormElement.prototype.reportValidity);
  Object.defineProperty(HTMLFormElement.prototype, 'noValidate', {
    enumerable: true, configurable: true,
    get() { return has(this, 'novalidate'); },
    set(v) { if (v) this.setAttribute('novalidate', ''); else this.removeAttribute('novalidate'); },
  });
  for (const C of [HTMLInputElement, HTMLButtonElement]) {
    Object.defineProperty(C.prototype, 'formNoValidate', {
      enumerable: true, configurable: true,
      get() { return has(this, 'formnovalidate'); },
      set(v) { if (v) this.setAttribute('formnovalidate', ''); else this.removeAttribute('formnovalidate'); },
    });
  }
  // requestSubmit / user submit: interactive validation (invalid events, focus
  // on the first failing control, no submit). Returns true when the submit may proceed.
  globalThis.__interactiveValidate = (form, submitter) => {
    if (form.noValidate || (submitter && submitter.formNoValidate)) return true;
    return formCheck(form, true);
  };
})();

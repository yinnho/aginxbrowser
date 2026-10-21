// CSS.registerProperty (CSS Properties & Values API), split from bootstrap.js:
// argument validation with the spec's error types, the registry, and the
// registered initial value as the computed value of an unset custom property
// (getComputedStyle falls through to __customPropInitial) and mirrored into
// the Rust cascade registry (diting_css/props.rs), which also owns @property
// rules: initial-value fallback, inherits:false and syntax validation. Typed
// animation is not modelled. Everything is call-time.
(() => {
  const reg = new Map();
  globalThis.__customPropInitial = (name) => { const r = reg.get(name); return r === undefined ? undefined : r.initial; };

  const NUM = '[+-]?(?:\\d+\\.?\\d*|\\.\\d+)(?:e[+-]?\\d+)?';
  const re = (s) => new RegExp('^' + s + '$', 'i');
  const ABS_LEN = re('(?:' + NUM + '(?:px|cm|mm|q|in|pt|pc)|0)');
  const TYPE_TEST = {
    'number': re(NUM),
    'integer': re('[+-]?\\d+'),
    'percentage': re(NUM + '%'),
    'length': ABS_LEN,
    'length-percentage': re('(?:' + NUM + '(?:px|cm|mm|q|in|pt|pc|%)|0)'),
    'angle': re('(?:' + NUM + '(?:deg|grad|rad|turn)|0)'),
    'time': re(NUM + '(?:s|ms)'),
    'resolution': re(NUM + '(?:dpi|dpcm|dppx|x)'),
    'color': /^(?:#[0-9a-f]{3,8}|[a-z]+|(?:rgb|hsl|hwb|lab|lch|oklab|oklch|color)a?\(.*\))$/i,
    'custom-ident': /^-?[_a-zA-Z\u0080-￿][\w\u0080-￿-]*$/,
    'image': /^(?:url|image-set|(?:repeating-)?(?:linear|radial|conic)-gradient)\(.*\)$/i,
    'url': /^url\(.*\)$/i,
    'transform-function': /^[a-z0-9]+\(.*\)$/i,
    'transform-list': /^(?:[a-z0-9]+\(.*\)\s*)+$/i,
  };
  const REL_UNIT = /(?:^|[\d.])(?:em|rem|ex|rex|cap|rcap|ch|rch|ic|ric|lh|rlh|vw|vh|vi|vb|vmin|vmax|svw|svh|lvw|lvh|dvw|dvh)$/i;
  const CSS_WIDE = /^(?:initial|inherit|unset|revert|revert-layer)$/i;

  // null = invalid syntax string; 'universal' for "*"; else the alternatives.
  const parseSyntax = (str) => {
    const s = str.trim();
    if (s === '*') return 'universal';
    if (!s) return null;
    const alts = [];
    for (const part of s.split('|')) {
      const p = part.trim();
      let m = /^<([a-z-]+)>([+#]?)$/.exec(p);
      if (m) {
        if (!Object.prototype.hasOwnProperty.call(TYPE_TEST, m[1])) return null;
        if (m[2] && m[1] === 'transform-list') return null;
        alts.push({ type: m[1], mult: m[2] });
      } else if (/^-?[_a-zA-Z\u0080-￿][\w\u0080-￿-]*$/.test(p) && !CSS_WIDE.test(p)) {
        alts.push({ literal: p });
      } else return null;
    }
    return alts;
  };

  const matches = (alts, value) => alts.some((a) => {
    if (a.literal !== undefined) return value === a.literal;
    const t = TYPE_TEST[a.type];
    if (!a.mult) return t.test(value) || /^calc\(.*\)$/i.test(value);
    const items = a.mult === '#' ? value.split(',') : value.split(/\s+/);
    return items.every((it) => t.test(it.trim()));
  });

  const err = (msg, name) => new DOMException(msg, name);
  CSS.registerProperty = function registerProperty(definition) {
    if (arguments.length < 1) throw new TypeError("Failed to execute 'registerProperty' on 'CSS': 1 argument required, but only 0 present.");
    if (definition === null || typeof definition !== 'object' && typeof definition !== 'function') {
      throw new TypeError("Failed to execute 'registerProperty' on 'CSS': The provided value is not of type 'PropertyDefinition'.");
    }
    const name = definition.name === undefined ? undefined : String(definition.name);
    if (name === undefined) throw new TypeError("Failed to execute 'registerProperty' on 'CSS': required member name is undefined.");
    if (definition.inherits === undefined) throw new TypeError("Failed to execute 'registerProperty' on 'CSS': required member inherits is undefined.");
    const syntax = definition.syntax === undefined ? '*' : String(definition.syntax);
    const inherits = !!definition.inherits;
    if (!/^--(?:[^\s\u0000-\u001f]|\\.)+$/.test(name) || name === '--') {
      throw err("Failed to execute 'registerProperty' on 'CSS': Custom property names must start with '--'.", 'SyntaxError');
    }
    if (reg.has(name)) throw err("Failed to execute 'registerProperty' on 'CSS': The name provided has already been registered.", 'InvalidModificationError');
    const alts = parseSyntax(syntax);
    if (alts === null) throw err("Failed to execute 'registerProperty' on 'CSS': The syntax provided is not a valid custom property syntax.", 'SyntaxError');
    let initial;
    if (definition.initialValue !== undefined) initial = String(definition.initialValue).trim();
    if (alts === 'universal') {
      if (initial === undefined) initial = '';
    } else {
      if (initial === undefined) throw err("Failed to execute 'registerProperty' on 'CSS': An initialValue must be provided if the syntax is not '*'.", 'SyntaxError');
      if (CSS_WIDE.test(initial) || /var\(/i.test(initial) || !matches(alts, initial)) {
        throw err("Failed to execute 'registerProperty' on 'CSS': The initial value provided does not parse for the given syntax.", 'SyntaxError');
      }
      if (REL_UNIT.test(initial) || /%$/.test(initial) && alts.some((a) => a.type === 'length')) {
        throw err("Failed to execute 'registerProperty' on 'CSS': The initial value provided is not computationally independent.", 'SyntaxError');
      }
    }
    reg.set(name, { syntax, inherits, initial });
    _OPS.op_register_property(name, syntax, inherits, initial);
  };
  _markNative(CSS.registerProperty);
})();

/**
 * Minimal DOM stub for `node --test` — just enough surface for the admin_ui
 * helpers (ui.js) and panel render functions to run headless. NOT a real DOM;
 * it models only what the code under test touches: element creation, class
 * lists, text/HTML content, children, and click listeners.
 *
 * Importing this module installs `globalThis.document` and `globalThis.HTMLElement`
 * as a side effect, so test files must import it BEFORE the modules under test.
 */

class ClassList {
    constructor() {
        this._set = new Set();
    }
    add(c) {
        this._set.add(c);
    }
    remove(c) {
        this._set.delete(c);
    }
    contains(c) {
        return this._set.has(c);
    }
    toggle(c) {
        if (this._set.has(c)) {
            this._set.delete(c);
            return false;
        }
        this._set.add(c);
        return true;
    }
    toString() {
        return [...this._set].join(' ');
    }
}

class HTMLElement {
    constructor(tag = 'div') {
        this.tagName = String(tag).toUpperCase();
        this.children = [];
        this._listeners = {};
        this.classList = new ClassList();
        this._text = '';
        this._html = '';
        this.attributes = {};
    }
    set className(v) {
        this.classList = new ClassList();
        String(v)
            .split(/\s+/)
            .filter(Boolean)
            .forEach((c) => this.classList.add(c));
    }
    get className() {
        return this.classList.toString();
    }
    set textContent(v) {
        this._text = v == null ? '' : String(v);
        this.children = [];
    }
    get textContent() {
        return this._text;
    }
    set innerHTML(v) {
        this._html = v == null ? '' : String(v);
    }
    get innerHTML() {
        return this._html;
    }
    appendChild(node) {
        this.children.push(node);
        return node;
    }
    setAttribute(k, v) {
        this.attributes[k] = String(v);
    }
    getAttribute(k) {
        return Object.prototype.hasOwnProperty.call(this.attributes, k) ? this.attributes[k] : null;
    }
    addEventListener(type, fn) {
        (this._listeners[type] ||= []).push(fn);
    }
    hasListener(type) {
        return (this._listeners[type] || []).length > 0;
    }
    /** Test helper: fire all listeners of `type` with a stub event. */
    dispatch(type, ev = {}) {
        (this._listeners[type] || []).forEach((fn) => fn({ preventDefault() {}, ...ev }));
    }
    querySelector(selector) {
        // Simple selector support: 'tag' or 'tag.class'
        if (!selector) return null;
        if (selector === 'tbody') return this.tagName === 'TBODY' ? this : this.children.find(c => c.tagName === 'TBODY');
        if (selector === 'thead') return this.tagName === 'THEAD' ? this : this.children.find(c => c.tagName === 'THEAD');
        if (selector === '.modal-overlay') return this.classList.contains('modal-overlay') ? this : null;
        return null;
    }
    querySelectorAll(selector) {
        // Simple selector support: 'tag' or 'tag.class'
        if (!selector) return [];
        if (selector === 'tr') {
            const result = [];
            if (this.tagName === 'TR') result.push(this);
            this.children.forEach(c => {
                if (c.tagName === 'TR') result.push(c);
            });
            return result;
        }
        return [];
    }
    removeChild(node) {
        const idx = this.children.indexOf(node);
        if (idx >= 0) {
            this.children.splice(idx, 1);
        }
        return node;
    }
}

class DocumentStub {
    constructor() {
        this.body = new HTMLElement('body');
    }
    createElement(tag) {
        return new HTMLElement(tag);
    }
    createTextNode(text) {
        const n = new HTMLElement('#text');
        n.textContent = text;
        return n;
    }
    getElementById() {
        return null;
    }
    querySelectorAll() {
        return [];
    }
    addEventListener(type, fn) {
        // Listen on the global document for keydown events (modal.js)
        (this._listeners ||= {})[type] ||= [];
        this._listeners[type].push(fn);
    }
    removeEventListener(type, fn) {
        if (!this._listeners || !this._listeners[type]) return;
        const idx = this._listeners[type].indexOf(fn);
        if (idx >= 0) {
            this._listeners[type].splice(idx, 1);
        }
    }
    dispatch(type, ev = {}) {
        if (!this._listeners || !this._listeners[type]) return;
        this._listeners[type].forEach((fn) => fn({ key: 'Escape', ...ev }));
    }
}

const document = new DocumentStub();

globalThis.HTMLElement = HTMLElement;
globalThis.document = document;

export { HTMLElement, ClassList, document, DocumentStub };

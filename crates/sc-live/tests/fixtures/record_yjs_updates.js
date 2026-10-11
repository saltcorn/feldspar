// Records the Yjs updates in `yjs_updates.json`, made by the JS `yjs` package, for
// `sc-live`'s format test (`tests/doc_format.rs`). To re-record, in an empty directory:
//   npm install yjs@13 && node record_yjs_updates.js > yjs_updates.json

function record(build) {
  const a = new Y.Doc(); a.clientID = 101;
  const b = new Y.Doc(); b.clientID = 202;
  const updates = [];
  a.on('update', (u, o) => { if (o === 'remote') return; updates.push(b64(u)); Y.applyUpdate(b, u, 'remote'); });
  b.on('update', (u, o) => { if (o === 'remote') return; updates.push(b64(u)); Y.applyUpdate(a, u, 'remote'); });
  const root = build(a, b);
  return { updates, state: b64(Y.encodeStateAsUpdate(a)), root };
}

// A text root, as CodeMirror binds it: inserts and deletes from two clients.
const text = record((a, b) => {
  const ta = a.getText('default'), tb = b.getText('default');
  ta.insert(0, 'Hello world');
  tb.insert(5, ',');
  ta.delete(7, 5);
  tb.insert(tb.length, 'Yjs and yrs');
  ta.insert(0, 'Grüße 👋 ');
  return ta;
});

// An xml root, as TipTap/ProseMirror binds it: paragraphs and a heading, one with bold text.
const xml = record((a, b) => {
  const fa = a.getXmlFragment('default'), fb = b.getXmlFragment('default');
  const h = new Y.XmlElement('heading'); h.setAttribute('level', '1');
  const ht = new Y.XmlText(); h.insert(0, [ht]); fa.insert(0, [h]); ht.insert(0, 'Notes');
  const p1 = new Y.XmlElement('paragraph'); const t1 = new Y.XmlText(); p1.insert(0, [t1]);
  fa.insert(1, [p1]); t1.insert(0, 'First line, ');
  t1.insert(t1.length, 'bold', { bold: true });
  const p2 = new Y.XmlElement('paragraph'); const t2 = new Y.XmlText(); p2.insert(0, [t2]);
  fb.insert(fb.length, [p2]); t2.insert(0, 'Second paragraph by Bob');
  const p3 = new Y.XmlElement('paragraph'); fb.insert(fb.length, [p3]);
  return fa;
});

const out = {
  yjs_version: require('yjs/package.json').version,
  text: { root: 'default', updates: text.updates, state: text.state,
          text: text.root.toString(), json: JSON.stringify(text.root.toJSON()) },
  xml: { root: 'default', updates: xml.updates, state: xml.state,
         xml: xml.root.toString(), json: JSON.stringify(xml.root.toJSON()) },
};
console.log(JSON.stringify(out, null, 2));

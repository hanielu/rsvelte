// Regenerates the data the language server's Emmet port embeds, and the
// oracle its tests compare against, from the packages the official language
// server completes Emmet with.
//
//   node scripts/dev/generate-emmet-data.mjs --package-root <node_modules>
//
// `<node_modules>` is the official server's dependencies, the directory
// holding `emmet` and `@vscode/emmet-helper` (for Zed's Svelte extension on
// macOS: `~/Library/Application Support/Zed/extensions/work/svelte/node_modules`).
//
// - `src/emmet/data.json`: the `html` snippet registry, the variables,
//   emmet-helper's commonly used tags and the lorem ipsum vocabularies.
// - `tests/data/emmet-oracle.json`: emmet-helper's `doComplete(document,
//   position, 'html', {})` — exactly what the official HTML plugin asks — on
//   each line of the corpus below. An item whose expansion differs between
//   two runs (lorem ipsum) is marked `random`.
import fs from "node:fs";
import path from "node:path";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";

const ROOT = path.resolve(fileURLToPath(import.meta.url), "../../..");
const CRATE = path.join(ROOT, "crates/rsvelte_language_server");
const DATA = path.join(CRATE, "src/emmet/data.json");
const ORACLE = path.join(CRATE, "tests/data/emmet-oracle.json");

/// Each line is completed at `‸`, or at its end.
const CORPUS = [
  // Tags, and the commonly used tags a prefix also offers.
  "div", "a", "p", "span", "ul", "img", "input", "br", "hr", "link", "meta", "script",
  "style", "body", "html", "head", "title", "button", "form", "label", "select", "option",
  "table", "tr", "td", "video", "audio", "source", "iframe", "textarea", "nav", "header",
  "footer", "section", "article", "aside", "main", "h1", "b", "i", "em", "strong", "li",
  "ol", "dl", "s", "t", "h", "l", "m", "n", "o", "c", "d", "f", "q", "u", "v", "w",
  "DIV", "Div", "A", "UL>LI", "Lorem",
  // Snippets.
  "!", "!!!", "html:5", "doc", "doc4", "a:link", "a:mail", "a:tel", "a:blank", "link:css",
  "link:favicon", "input:text", "input:email", "input:c", "inp", "btn", "btn:s", "btn:r",
  "select+", "ul+", "ol+", "dl+", "table+", "tr+", "map+", "bq", "fig", "figc", "pic",
  "src", "sect", "art", "hdr", "ftr", "emb", "obj", "ifr", "cmd", "kg", "tarea", "out",
  "det", "opt", "optg", "leg", "fst", "fset", "adr", "acr", "dlg", "menu:c", "c", "cc:ie",
  "cc:noie", "ri:art", "ri:d", "ri:v", "ri:t", "ri:a", "html:4t", "html:xt", "html:xs",
  "meta:vp", "meta:compat", "script:src", "img:s", "img:z", "form:get", "label+",
  "mn", "bdo:r", "area:c", "base", "basefont", "col", "embed", "colg+",
  // Attributes.
  "div#app", "div.a.b", "div#a.b.c", ".foo", "#bar", "div[data-x=1]", "a[href=x]",
  'div[title="hello world"]', "div[title='a b']", "input[disabled]",
  "input[type=checkbox checked]", "[]", "div[]", "[a]", "p[!href]", "a[!href=x]",
  "span[x.]", "div[a=b c=d]", 'div[a="b"c]', "div[a=]", 'div[="x"]', 'div["x"]',
  "div[x=${y}]", "div[a={b}]", "div.a.a", "div.a.b.a", "div..x", "div##x", "div.",
  "div[title=${1:hi}]", "a[href=${0}]", "html[lang=${lang}]", "meta[charset=${charset}]",
  "div[a=b]#x[a=c]", "div[class=x].y", "input[required]", "select[multiple]",
  // Text.
  "p{hi}", "p{hello world}", "div{a}+div{b}", "{text}", "p{a{b}c}", "div{}", "a{click}",
  "div{${1:x}}", "div{${1}}>p", "div{before ${1} after}>p*2", "{<div>${0}</div>}",
  "div{<div>x</div>}", "div{<span>x</span>}", "{x}>p", "{a}+{b}", "{x}*2", "div{\\}}",
  "div{a\\{b}", "div{\\$x}", "div{a\\$}", "div{$5}", "div{price: 5$}", "span{x}+div",
  "p{x}*3", "div{x}*3",
  // Nesting and repeating.
  "ul>li*3", "ul>li.item$*3", "ul>li.item$$*3", "ul>li.item$@-*3", "ul>li.item$@3*3",
  "ul>li*3>a", "div>(header>ul>li*2>a)+footer>p", "(div>p)*2", "table>tr*2>td*2",
  "nav>ul>li*3>a[href=#]{Link $}", "ul>li*", "div>p>span^p", "div>p>span^^p", "h$*3",
  "div+p+bq", "a>b", "p>a", "div>.inner", "ul>.item*2", "table>.row>.cell", "select>.opt",
  "span>.x", "em>.x", "ul>li*>a{$#}", "ul>li{item $#}*", "div.item$$$*2",
  "div[id=item$]*2", "ul>li*2>ul>li$@^*2", "ul>li*2>span$@^", "(div)*2", "(div>p)+p",
  "(div)>p", "div>(p)", "div^p", "div>p^^^^span", "a*0", "ul>li*3>{x$}", "div*1001",
  "div>p*2>span", "section>(h2+p)*2", "div>(ul>li)*2", "dl>(dt+dd)*2", "div*2>p",
  "ul>li*2>a+span", "p>span+em+b", "div>span+span+span", "div>b+div", "p>span+div",
  "a+b+i+em", "div>span{x}", "span+span+span", "html>head+body", "html>body", "body>div",
  "body", "div>body", "br/", "div/", "img/", "input/", "div/*2", "div/>p", "ul>.x",
  "p>.x", "table>.x>.y", "audio>.x", "map>.x", "object>.x", "colgroup>.x", "a>.x",
  ".x>.y", "div>[a]", "label>input", "label[for=x]>input", "label>input[id=y]",
  "label>textarea", "label>div>input", "label[for]>input", "svelte:head", "svelte:window",
  "my:comp", "foo:bar", "input:t", "a:", "x-y", "div--x", "foo::bar", "x-",
  // Lorem ipsum.
  "lorem", "lorem5", "lorem10-12", "p*2>lorem4", "loremru5", "loremsp3", "ul>lorem3*2",
  "lorem0", "lorem-", "p>lorem2", "lorem2*2",
  // Filters.
  "div|c", "div#a.b|c", ".a|c", "div.b>.-e|bem", "div.b>.-e+._m|bem", "div|t",
  "ul>li*2|c", "div#x|bem|c", "div|t|c", ".b__e_m|bem", ".b>.-e>.--f|bem",
  "div.b_m|bem", "div.blk>div.-el._mod|bem", "p.a-b>._m|bem",
  // Noise and invalid abbreviations.
  "hello", "Hello", "MyComp", "my-comp", "hello.", ".", "abc.", "(hello)", "(div)",
  'span[onclick="alert();"]', "div(", "!!", "!a", "123", "div.foo.", "div>", "div+",
  "div^", ">div", "+div", "*div", "^p", "div*", "x", "hellodiv", "constructor",
  // Lines with more than the abbreviation, and the cursor inside them.
  "<p>some text div‸</p>", "  div", "\tul>li", "<div>div‸", "text div‸ more",
  '<div class="x">p‸', "<br/>p", '<a href="x">em', "<div>ul>li*2‸</div>",
  "div[title=x‸]", "div{te‸}", 'div[title="x‸"]', "p{hi‸}", "<div", "<div>‸", "</div>‸",
  "<!-- x -->‸", "<!-- x -->div", "<span>.foo[title=bar‸]</span>", "a b c div",
  "<p>héllo div‸</p>", "日本 div", "😀 p", "😀😀 ul>li*2", "x=div", "x'div", "(div‸)",
  "<img src=x>div", '<div a="b">p', "<div a=b>p", "<div a>p", "<x-y>p", "<a:b>p",
  "{#if x}div", "{x}div", "div‸>p", "di‸v", "div.a‸.b", "ul>li*3‸ ", "<div/>p",
];

function packageRoot() {
  const flag = process.argv.indexOf("--package-root");
  if (flag === -1 || !process.argv[flag + 1]) {
    throw new Error("pass --package-root <the official server's node_modules>");
  }
  const root = path.resolve(process.argv[flag + 1]);
  for (const name of ["emmet", "@vscode/emmet-helper", "vscode-languageserver-textdocument"]) {
    if (!fs.existsSync(path.join(root, name, "package.json"))) {
      throw new Error(`${name} is not installed in ${root}`);
    }
  }
  return root;
}

function data(require, root) {
  const emmet = require(path.join(root, "emmet"));
  const helperData = require(path.join(root, "@vscode/emmet-helper/lib/cjs/data.js"));
  // The vocabularies are not exported; they are object literals in the bundle.
  const source = fs.readFileSync(path.join(root, "emmet/dist/emmet.cjs"), "utf8");
  const vocabulary = (name) => {
    const start = source.indexOf(`var ${name} = {`);
    const end = source.indexOf("\n};", start);
    if (start === -1 || end === -1) {
      throw new Error(`no ${name} vocabulary in emmet.cjs`);
    }
    return JSON.parse(source.slice(start + `var ${name} = `.length, end + 2));
  };
  const config = emmet.resolveConfig({ type: "markup", syntax: "html" });
  return {
    emmet: require(path.join(root, "emmet/package.json")).version,
    helper: require(path.join(root, "@vscode/emmet-helper/package.json")).version,
    snippets: Object.entries(config.snippets),
    variables: Object.entries(config.variables),
    tags: [...helperData.htmlData.tags, "lorem"],
    lorem: { latin: vocabulary("latin"), ru: vocabulary("ru"), sp: vocabulary("sp") },
  };
}

function oracle(require, root) {
  const { doComplete } = require(path.join(root, "@vscode/emmet-helper"));
  const { TextDocument } = require(path.join(root, "vscode-languageserver-textdocument"));
  const complete = (text, character) => {
    const document = TextDocument.create("file:///corpus.html", "html", 0, text);
    return doComplete(document, { line: 0, character }, "html", {}) ?? null;
  };
  return CORPUS.map((marked) => {
    const at = marked.indexOf("‸");
    const line = marked.replace("‸", "");
    // The cursor in UTF-16 code units, as an LSP position counts it.
    const cursor = at === -1 ? line.length : at;
    const first = complete(line, cursor);
    const second = complete(line, cursor);
    const items = first?.items.map((item, index) => ({
      label: item.label,
      newText: item.textEdit.newText,
      documentation: item.documentation,
      start: item.textEdit.range.start.character,
      end: item.textEdit.range.end.character,
      random: item.textEdit.newText !== second?.items[index]?.textEdit.newText,
      detail: item.detail,
      kind: item.kind ?? null,
      insertTextFormat: item.insertTextFormat,
      filterText: item.filterText ?? null,
      sortText: item.sortText ?? null,
    }));
    return { line, cursor, isIncomplete: first?.isIncomplete ?? null, items: items ?? null };
  });
}

const root = packageRoot();
const require = createRequire(import.meta.url);
fs.writeFileSync(DATA, `${JSON.stringify(data(require, root), null, 1)}\n`);
fs.writeFileSync(ORACLE, `${JSON.stringify(oracle(require, root), null, 1)}\n`);
console.log(`wrote ${path.relative(ROOT, DATA)} and ${path.relative(ROOT, ORACLE)}`);

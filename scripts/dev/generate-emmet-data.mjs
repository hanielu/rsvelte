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
//   emmet-helper's commonly used tags and the lorem ipsum vocabularies; the
//   stylesheet snippet registry, keywords and unitless properties, and
//   emmet-helper's CSS property names.
// - `tests/data/emmet-oracle.json`: emmet-helper's `doComplete(document,
//   position, 'html', {})` — exactly what the official HTML plugin asks — on
//   each line of the corpus below. An item whose expansion differs between
//   two runs (lorem ipsum) is marked `random`.
// - `tests/data/emmet-stylesheet-oracle.json`: `lines`, the same
//   `doComplete` for `css`, `scss`, `less` and `sass` on each stylesheet line
//   below and every stylesheet snippet name; `documents`, the official CSS
//   plugin's own `getCompletions` on each component below, its Emmet items
//   and `isIncomplete` — where it asks emmet-helper, and what it answers.
import fs from "node:fs";
import path from "node:path";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";

const ROOT = path.resolve(fileURLToPath(import.meta.url), "../../..");
const CRATE = path.join(ROOT, "crates/rsvelte_language_server");
const DATA = path.join(CRATE, "src/emmet/data.json");
const ORACLE = path.join(CRATE, "tests/data/emmet-oracle.json");
const STYLESHEET_ORACLE = path.join(CRATE, "tests/data/emmet-stylesheet-oracle.json");

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

/// Each line is completed at `‸`, or at its end, as `css`, `scss`, `less`
/// and `sass`.
const STYLESHEET_CORPUS = [
  // Numbers and units.
  "m10", "m-10", "m10-20", "m10-20-30-40", "m10-20-30-40-50", "m.5", "m1.5", "m1.", "m-.5",
  "m0", "m-0", "m0-a", "p10p", "p10e", "p10x", "p10r", "p10em", "p10vh", "w100p", "h50vh",
  "w1/2", "lh1.5", "lh2", "z10", "z-1", "op.5", "op1", "fw700", "zoom2", "zom2", "fx1", "fxg1",
  "fxsh0", "m10!", "p0!", "m10-auto", "m10-a", "ma", "m:a", "m-a", "m:10", "m:10px", "m10px",
  "m10+p20", "m10+p20+bd", "m10-20!", "bdrs50p", "bdrs5-10", "t0+l0", "fz12", "fz1.2",
  "fz1.2r", "fz1.25", "lts1", "lts.5", "m00", "m010", "m10.10", "m0.03125", "m-0.03125",
  "w1e3", "m99999999999999999999999", "m1234567890123", "ord1", "m10-", "m-10-", "m--10",
  // Colours.
  "c#f", "c#fc0", "c#fff", "c#ffcc00", "c#f.5", "c#t", "c#t.5", "c#", "c#.5", "c#.", "bgc#fff",
  "bd1-s#f", "bd1-s-#f00", "c#0", "c#00", "c#000", "c#abc.3", "c#abc.33333", "bgc#f.",
  "c#1234", "c#12345", "c#1234567", "bd#f00", "c:#f", "c#F", "c#ABC", "c#f0f", "c#ff00ff",
  "c#f.001953125", "#f", "#fff", "#ffffff", "#ff", "#fffffff", "#g", "#", "#f.5",
  // Keywords and snippet values.
  "dib", "db", "dn", "df", "dif", "dg", "dt", "pos:a", "posa", "posr", "pos:s", "fl:l", "fll",
  "fr", "cl", "clb", "ovh", "ovs", "tac", "tar", "tal", "taj", "fwb", "fsi", "ttu", "tdn",
  "td:n", "td:u", "jcc", "aic", "jcsb", "fxdc", "fxww", "cur:p", "curp", "bxz", "bxzbb",
  "bdn", "bd+", "bd", "bds", "bdcl", "lis:n", "lisn", "us:n", "ws:nw", "whsnw", "va:m", "vam",
  "trf", "trf:r", "trf:t", "trf:s", "trf:sc", "trs", "trs:a", "anim", "animn", "anm", "bg",
  "bg+", "bgi", "bgr", "bgrn", "bgp", "bgs", "bgsz:cv", "bgszcv", "bgcl", "bga", "ff", "ff:s",
  "ff:ss", "ff:m", "fz", "lts", "wob", "wow", "ap", "ap:n", "ol", "olw", "ols", "ct", "ct:a",
  "cnt", "q", "ori", "zom", "zm", "ov:h", "ovx", "ovy", "objf", "objf:c", "gtc", "gtr", "ga",
  "gg", "gap", "rg", "cg", "pe", "pen", "ac", "ai", "as", "ord", "fx", "fxg", "flex",
  "fx1-1-a", "bdrs", "bdrs5", "bxsh", "bxsh:n", "tsh", "tsh:n", "tsh:r", "c", "o", "op", "v",
  "vis", "vis:h", "bdc", "bdc#f", "brd", "bdt", "bdb+", "bdl:n", "w", "h", "wid", "mah",
  "maw", "mih", "miw", "t", "b", "l", "r", "p", "pt", "pr10", "pb", "pl", "mt", "mr", "mb",
  "ml", "m", "d", "f", "g", "x", "y", "z", "fs", "fst", "fv", "fw", "ffa", "lh", "ta", "te",
  "tt", "ti", "tov", "tj", "wfsm", "cps", "tbl", "bdcl:c", "ec", "ec:s", "scr", "sp",
  "animdur", "animdur2", "animdel", "animtf", "animtf:cb", "fxb", "fxf", "fxd", "fxw",
  "jc", "jcs", "aisb", "ais", "acc", "jcse", "jcsa", "jcfe", "pos", "posf", "ov", "z0",
  "dib+dn", "dib!", "pos:a!", "trf:t!", "bgc", "bgc:t", "bgi:n", "bgie", "bgo", "bgpx",
  "bgpy", "bgcp", "bga:f", "bdf", "bdi", "bdbk", "bdst", "bdts", "bdtw", "bdtc", "bdbrrs",
  "bdblrs", "bdtrrs", "bdtlrs", "bdrz", "bdsp", "bdj", "bdw", "bdw1", "bdc:t", "trf:rx",
  "trf:skx", "trf:tx", "trf:r10", "trf:t10-20", "trf:s1.5", "trs:t", "trsde", "trsdu",
  "trsp", "trstf", "cm", "zi", "ovs:a", "ovsx", "ov:a", "mar", "pad", "bor", "col", "dis",
  "fon", "tex", "bac", "lin", "lin5", "lis", "lisp", "list", "q:n", "ori:h", "sb",
  // Gradients and functions.
  "lg", "lg(red, blue)", "lg(red,blue)", "lg(to right, #f, #0)", "lg()", "bgi:lg", "bg:lg",
  "bgi:lg(red)", "c:rgba(0,0,0,.5)", "w:calc(100%-10px)", "trf:scale3d(1,2,3)",
  "trf:t(10,20)", "rgba(m10", "(m10)", "m(10)", "m10)", "fn(m10)", "lg(", "calc(10px)",
  "url(x)", "m10()", "bgi:url(x.png)",
  // At-rules and raw snippets.
  "@m", "@media", "@i", "@import", "@f", "@ff", "@kf", "@char", "@cs", "@p", "@page",
  "@supports", "@s", "@n", "@", "@mx", "@x", "@font-face", "@m10", "@m+p10", "p10+@m",
  // !important.
  "!", "!i", "!imp", "!important", "!importantx", "m!", "!a", "!!", "!10",
  // Property names and long abbreviations.
  "width", "widt", "heig", "height", "color", "colo", "margin", "marg", "padding", "displ",
  "display", "position", "posit", "background", "backg", "font", "font-size", "fontsize",
  "border", "borde", "opacity", "transform", "flex-direction", "abcde", "abcdef", "xyzzy",
  "z-index", "zindex", "align", "align-items", "justify", "outline", "cursor", "trans",
  "colors", "widths", "textal", "overflow", "visib",
  // Noise and invalid abbreviations.
  "abc", "abc:d", "xyz", "foo", "a", "m:", "m-", "hello", "Hello", "M10", "DIB", "Dib", "ABC",
  "10", "10px", ".5", "-m10", "-x", "-webkit-", "-moz-bdrs", "--foo", "--", "-", "$foo",
  "##", "div#a", "p#a", "span#x", "a#b", "img#fff", "m#f", "lorem#f", "*", "m*2", "%", "/",
  "_", ":", "::", ":hover", "m10 ", " m10 ", "m10%", "m10%20", "m_10", "m10_20", "w$x",
  "m${x}", "m@x", "m$x", "c:$color", "c:@color", "m:$x", "m:@x", "m10'", "m'10'",
  'c:"red"', "ff:'Arial'", 'ff:"a b"', "ff:'a", "m10,20", "m10,", "c:red,blue", "m+",
  "+m10", "m10++p10", "m10-+p10", "m10:", "m::10", "m10>p", "m10^p", "a>b", "m10.", "m.",
  "m..5", "m:a:b", "m-a-b", "m:a-b", "m10-ab", "m10a", "dibx", "m10px20", "t-10", "t--10",
  // Filters.
  "m10|c", "m10|bem", "m10|t", "m10|c|t", "dib|c", "|c", "m10|", "m10|x",
  // Lines with more than the abbreviation, and the cursor inside them.
  "div { m10", "div{m10‸}", "  m10", "\tm10", "color: re", "color: red", "margin: 10px a",
  "margin: 0 a‸;", "m10;", "m10‸;", "a {m10", "@media x { m10", "p10‸ m20", "m10‸ ", "m1‸0",
  "<style>m10", "<div>m10", "x>m10", "content: 'm10", "m10'", '"m10"', "c#f.5‸)", "(m10",
  "<b>m10", "<br/>m10", "<a href='x'>m10", "<a href=x>m10", "<x y>m10", "日本 m10",
  "😀 m10", "é m10", "m10 /* x */", "/* m10", "// m10", "a:hover{m10", "&:hover{m10",
  "$x: m10", "@include m10", "m10 !important", "m10‸!", "!‸important", "di‸b", "m10‸px",
  "x;m10", "x,m10", "x(m10", "x)m10", "x*m10", "x+m10", "x-m10", "x.m10", "x:m10", "x@m10",
  "x#m10", "x!m10", "x%m10", "x^m10", "x_m10", "x/m10", "x$m10", "{m10}", "[m10]",
];

/// Each component is completed at `‸` by the official CSS plugin.
const STYLE = (body, attributes = "") => `<style${attributes}>\n${body}\n</style>`;
const STYLE_DOCUMENTS = [
  // Where the CSS language service asks its participants, in each language.
  ...["", ' lang="scss"', ' lang="less"', ' lang="postcss"', ' type="text/css"', ' lang="text/scss"']
    .flatMap((attributes) => [
      STYLE("div { m10‸ }", attributes),
      STYLE("div {\n  m10‸\n}", attributes),
      STYLE("div {\n  color: red;\n  p10-20‸\n}", attributes),
      STYLE("div { color: re‸ }", attributes),
      STYLE("div { pos:a‸ }", attributes),
      STYLE("m10‸", attributes),
      STYLE("@media screen { m10‸ }", attributes),
      STYLE("div { @m‸ }", attributes),
      STYLE("div { #fff‸ }", attributes),
      STYLE("div { color: #fff‸ }", attributes),
    ]),
  // Rules, at-rules and nesting.
  STYLE("div {\n  m10‸\n  color: red;\n}"), STYLE("div { color: red; m10‸ }"),
  STYLE("div { color: red;m‸ }"), STYLE("div { color: red;‸ }"), STYLE("div { ‸ }"),
  STYLE("div{dib‸}"), STYLE("div { m10‸; }"), STYLE("div { m10‸ }}"), STYLE("div { m10 p10‸ }"),
  STYLE("div { m10+p10‸ }"), STYLE("div { bd+‸ }"), STYLE("div { m:‸ }"), STYLE("div { m:a‸ }"),
  STYLE("div { trf:t‸ }"), STYLE("div { fw:b‸ }"), STYLE("div { w100p‸ }"),
  STYLE("div { c#f.5‸ }"), STYLE("div { lh1.5‸ }"), STYLE("div { --foo‸ }"), STYLE("div { !‸ }"),
  STYLE("div { color: red !‸ }"), STYLE("div { color: red !imp‸ }"), STYLE("div { color: red ‸ }"),
  STYLE("div { margin: 10px a‸ }"), STYLE("div { margin: 10px a‸; }"), STYLE("div { margin: 1‸0px }"),
  STYLE("div { margin: 10p‸ }"), STYLE("div { color: #f‸ }"), STYLE("div { color: c#f‸ }"),
  STYLE('div { content: "m10‸" }'), STYLE("div { content: 'm10‸' }"),
  STYLE("div { width: calc(10px + m‸) }"), STYLE("div { background: url(m‸) }"),
  STYLE("div { color: rgba(0, 0, 0, a‸) }"), STYLE("div { transform: rotate(a‸) }"),
  STYLE("div { a‸ }"), STYLE("div { d‸ }"), STYLE("div { pos‸ }"), STYLE("div { width‸ }"),
  STYLE("div { widt‸ }"), STYLE("div { colo‸ }"), STYLE("div { margin‸: 1px }"),
  STYLE("div { m‸: 1px }"), STYLE("div { m10‸: 1px }"), STYLE("div { margin: 1px; m‸ }"),
  STYLE("div { &:hov‸ }"), STYLE("div { .foo‸ }"), STYLE("div { &:hover { m10‸ } }"),
  STYLE("div { a:hover‸ }"), STYLE("div { a:hover { m10‸ } }"), STYLE("div { > p { m10‸ } }"),
  STYLE("div { m10‸\n  p10 {\n  }\n}"), STYLE("div {\n  m10‸\n  p10\n}"),
  STYLE(":global(div) { m10‸ }"), STYLE(":global { div { m10‸ } }"), STYLE("div, p { m10‸ }"),
  STYLE("div { color: red; } m10‸"), STYLE("div { color: red; }\nm10‸"), STYLE("}m10‸"),
  STYLE("div {\n  color: red;\n  \n  m10‸\n}"), STYLE("div { /* m10‸ */ }"),
  STYLE("/* div { */ m10‸"), STYLE("div { /* } */ m10‸ }"), STYLE("div { color: /* x */ re‸ }"),
  STYLE("div { color: red; /* m10‸ */ }"), STYLE("div { color: red;/* m10‸ */ }"),
  STYLE("@font-face { m10‸ }"), STYLE("@keyframes x { from { m10‸ } }"),
  STYLE("@keyframes x { fr‸ }"), STYLE("@keyframes x { 50% { m10‸ } }"),
  STYLE("@-webkit-keyframes x { fr‸ }"), STYLE("@keyframes x { to { m10‸ } fr‸ }".replace("‸", "")),
  STYLE("@supports (display: grid) { div { m10‸ } }"), STYLE("@supports (display: grid) { m10‸ }"),
  STYLE("@supports (m‸"), STYLE("@media (min-width: 10px) { m10‸ }"), STYLE("@media (m‸"),
  STYLE("@media (min-width: m‸) {}"), STYLE("@layer x { m10‸ }"), STYLE("@layer m‸"),
  STYLE("@container (min-width: 1px) { m10‸ }"), STYLE("@page { m10‸ }"),
  STYLE("@page :first { m10‸ }"), STYLE("@import m‸"), STYLE("@m‸"), STYLE("@media‸"),
  STYLE("div { @media screen { m10‸ } }"), STYLE("div { @include m‸ }"),
  STYLE("div { @apply m‸ }"), STYLE("div { @m10‸ }"), STYLE("@media screen {\n  m10‸\n  div {}\n}"),
  STYLE("div {\n  m10‸"), STYLE("div {\n  m10‸\n"), STYLE("div { m10‸ } p { p10 }"),
  STYLE("div { color: red; margin: 1px; p10‸ }"), STYLE("div { color: red } p { m10‸ }"),
  STYLE("div {\n\tm10‸\n}"), STYLE("div {\r\n  m10‸\r\n}"), STYLE("  div { m10‸ }"),
  STYLE("div { m10‸ }\n\n\n"), STYLE("a[href] { m10‸ }"), STYLE("a[href='x'] { m10‸ }"),
  STYLE('a[title="{"] { m10‸ }'), STYLE("a::before { m10‸ }"), STYLE("div { content: '{'; m10‸ }"),
  STYLE('div { content: "}"; m10‸ }'), STYLE("div { --x: { a: b }; m10‸ }"),
  STYLE("div { --x: m‸ }"), STYLE("div { --x: 10px; p‸ }"), STYLE("div { color: var(--x) m‸ }"),
  STYLE("div { color: var(m‸) }"), STYLE("div { grid-template-areas: 'a b' m‸ }"),
  STYLE("div { font: 12px/1.5 a‸ }"), STYLE("div { margin: 0 !important; m10‸ }"),
  STYLE("div { m10‸ !important }"), STYLE("div { m10 !‸ }"), STYLE("div { dib‸ ;}"),
  STYLE("div {\n  m10‸ // x\n}", ' lang="scss"'), STYLE("div { // m10‸\n}", ' lang="scss"'),
  STYLE("div { // }\n  m10‸\n}", ' lang="scss"'), STYLE("div { // x\n  m10‸\n}", ' lang="less"'),
  STYLE("div { // m10‸\n}"),
  // SCSS and LESS.
  STYLE(".a { .b { m10‸ } }", ' lang="scss"'), STYLE("div { $x‸ }", ' lang="scss"'),
  STYLE("div { $x: m‸ }", ' lang="scss"'), STYLE("$x: m‸;", ' lang="scss"'),
  STYLE("div { m10‸ }\n$x: 1;", ' lang="scss"'), STYLE("div { &__el { m10‸ } }", ' lang="scss"'),
  STYLE("div { &:hover { m10‸ } }", ' lang="scss"'), STYLE("@mixin x { m10‸ }", ' lang="scss"'),
  STYLE("@include x { m10‸ }", ' lang="scss"'), STYLE("div { @include m‸ }", ' lang="scss"'),
  STYLE("div { @include x; m10‸ }", ' lang="scss"'), STYLE("div { @extend .m‸ }", ' lang="scss"'),
  STYLE("div { m: $x‸ }", ' lang="scss"'), STYLE("div { #{$x}: m‸ }", ' lang="scss"'),
  STYLE("div { #{m‸} }", ' lang="scss"'), STYLE("div { width: #{m‸} }", ' lang="scss"'),
  STYLE("div { font: { m‸ } }", ' lang="scss"'), STYLE("div { font: { family: a‸ } }", ' lang="scss"'),
  STYLE("@function f() { m10‸ }", ' lang="scss"'), STYLE("@if $x { m10‸ }", ' lang="scss"'),
  STYLE("div { @if $x { m10‸ } }", ' lang="scss"'), STYLE("@each $i in a { m10‸ }", ' lang="scss"'),
  STYLE("%x { m10‸ }", ' lang="scss"'), STYLE("div { m10‸ }", ' lang="scss"'),
  STYLE(".a { .mixin‸ }", ' lang="less"'), STYLE(".a { .mixin(); m10‸ }", ' lang="less"'),
  STYLE("@x: m‸;", ' lang="less"'), STYLE("div { @x: m‸; }", ' lang="less"'),
  STYLE("div { m: @x‸ }", ' lang="less"'), STYLE("div { @{m‸} }", ' lang="less"'),
  STYLE(".a when (m‸) {}", ' lang="less"'), STYLE(".a { &:extend(.b); m10‸ }", ' lang="less"'),
  // `@supports` conditions, colours in values, LESS mixins and interpolation.
  STYLE("@supports (display: g‸"), STYLE("@supports not (m‸"), STYLE("@supports (m‸) {}"),
  STYLE("div { @supports (m‸ }"), STYLE("@supports (a: b) and (m‸"), STYLE("@supports selector(m‸"),
  STYLE("@supports ((m‸"), STYLE("@media screen { @supports (m‸) {} }"),
  STYLE("div { color: #ff‸ }"), STYLE("div { color: #ffff‸ }"), STYLE("div { color: #fffff‸ }"),
  STYLE("div { color: #ffffffff‸ }"), STYLE("div { color: #g‸ }"), STYLE("div { color: #fg‸ }"),
  STYLE("div { color: #‸ }"), STYLE("div { color: #fffg‸ }"), STYLE("div { b: 1px solid #ff‸ }"),
  STYLE("div { b: 1px solid #fff‸ }"), STYLE("div { color: #fff !imp‸ }"),
  STYLE("div { #f‸ }", ' lang="less"'), STYLE("div { #ns.m‸ }", ' lang="less"'),
  STYLE("div { #ns > m‸ }", ' lang="less"'), STYLE("div { color: @{m‸} }", ' lang="less"'),
  STYLE("div { @{m‸}: 1px }", ' lang="less"'), STYLE("div { m@{x}‸ }", ' lang="less"'),
  STYLE("div { @{x}m‸ }", ' lang="less"'), STYLE("@{m‸}", ' lang="less"'),
  STYLE("div { .m‸ }", ' lang="less"'), STYLE("div { &:extend(.m‸) }", ' lang="less"'),
  STYLE("div { m#{x}‸ }", ' lang="scss"'), STYLE("div { #{x}m‸ }", ' lang="scss"'),
  // Parentheses after a property's name, and in a value.
  STYLE("div { lg(r‸ }"), STYLE("div { lg(red,b‸ }"), STYLE("div { :is(m‸) }"),
  STYLE("div { a:is(m‸) }"), STYLE("div { m(a‸) }"), STYLE("div { color: rgb(a‸ }"),
  // A component's styles as they are usually written.
  STYLE("  .card {\n    display: flex;\n    gap: 1rem;\n    p1‸\n  }\n\n  .title {\n    fz2r\n  }"),
  STYLE("  .card {\n    display: flex;\n  }\n\n  .title {\n    fz2r‸\n  }"),
  STYLE("  :global(body) {\n    m0‸\n  }"), STYLE("  .a {\n    &:hover {\n      c#f00‸\n    }\n  }", ' lang="scss"'),
  STYLE("  .a {\n    transition: opacity 0.2s e‸\n  }"), STYLE("  .a {\n    bd1-s#ccc‸\n  }"),
  // Stylus, Sass and other languages.
  STYLE("div { m10‸ }", ' lang="stylus"'), STYLE("div\n  m10‸", ' lang="styl"'),
  STYLE("div\n  m10‸", ' lang="sass"'), STYLE("m10‸", ' lang="sass"'), STYLE("div\n  width‸", ' lang="sass"'),
  STYLE("div\n  color: re‸", ' lang="sass"'), STYLE("div\n  pos:a‸", ' lang="sass"'),
  STYLE("div\n  @m‸", ' lang="sass"'), STYLE("div\n  c#f.5‸", ' lang="sass"'),
  STYLE("div\n  // m10‸", ' lang="sass"'), STYLE("div\n  m10‸", ' type="text/sass"'),
  "<style lang=\"sass\">m10‸</style>", "<style lang=\"sass\">div m10‸</style>",
  STYLE("div { m10‸ }", ' lang="foo"'), STYLE("div { m10‸ }", ' lang="CSS"'),
  STYLE("div { m10‸ }", ' lang="SCSS"'), STYLE("div { m10‸ }", ' type="text/less"'),
  // The style element itself.
  "<style>div { m10‸ }</style>", "<style>m10‸</style>", "<style>div{m10‸}</style>",
  "<style>\ndiv { m10‸ }</style>", "<style>div { m10‸ }\n</style>", "<style>div {m10‸</style>",
  "<p>a</p><style>div { m10‸ }</style>", "<script>let a;</script>\n<style>\n  div { m10‸ }\n</style>",
  "<div>x</div>\n<style>\n  .a { p10‸ }\n</style>\n<div>y</div>",
  "<style>\n  div { m10‸ }\n</style>\n<style>\n  p { p10 }\n</style>",
  "<style>\n  div { m10 }\n</style>\n<style>\n  p { p10‸ }\n</style>",
  "<svelte:head><style>div { m10‸ }</style></svelte:head>",
  "<div><style>div { m10‸ }</style></div>", "{#if x}<style>div { m10‸ }</style>{/if}",
  "<style global>div { m10‸ }</style>", "<style lang=scss>div { m10‸ }</style>",
  "<style\n  lang=\"scss\"\n>div { m10‸ }</style>", "<style>😀{} div { m10‸ }</style>",
  "<style>日本 { m10‸ }</style>", "<style>div { é: 1; m10‸ }</style>",
  "<style>div { m10‸ }</style><p>m10</p>",
  // Style attributes.
  '<div style="m10‸"></div>', "<div style='m10‸'></div>", '<div style="color: red; p10‸"></div>',
  '<div style="color: re‸"></div>', '<div style="‸"></div>', '<div style="m10‸;"></div>',
  '<div style="dib‸" class="x"></div>', '<div class="x" style="pos:a‸"></div>',
  '<div style="\n  m10‸\n"></div>', '<div style="color: red;\n  p10‸"></div>',
  '<div style="color: {c}; m10‸"></div>', '<div style={`m10‸`}></div>',
  '<div style:color="m10‸"></div>', '<Comp style="m10‸" />', '<div style="#fff‸"></div>',
  '<div style="@m‸"></div>', '<div style="m10+p10‸"></div>', '<div style=m10‸></div>',
  '<div title="m10‸"></div>', '<p>m10‸</p>',
];

function packageRoot() {
  const flag = process.argv.indexOf("--package-root");
  if (flag === -1 || !process.argv[flag + 1]) {
    throw new Error("pass --package-root <the official server's node_modules>");
  }
  const root = path.resolve(process.argv[flag + 1]);
  for (const name of [
    "emmet",
    "@vscode/emmet-helper",
    "vscode-languageserver-textdocument",
    "svelte-language-server",
  ]) {
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
  const stylesheet = emmet.resolveConfig({ type: "stylesheet", syntax: "css" });
  return {
    emmet: require(path.join(root, "emmet/package.json")).version,
    helper: require(path.join(root, "@vscode/emmet-helper/package.json")).version,
    snippets: Object.entries(config.snippets),
    variables: Object.entries(config.variables),
    tags: [...helperData.htmlData.tags, "lorem"],
    lorem: { latin: vocabulary("latin"), ru: vocabulary("ru"), sp: vocabulary("sp") },
    stylesheet: {
      snippets: Object.entries(stylesheet.snippets),
      keywords: stylesheet.options["stylesheet.keywords"],
      unitless: stylesheet.options["stylesheet.unitless"],
      properties: helperData.cssData.properties,
    },
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

/// An item as the tests compare it.
function item(item) {
  return {
    label: item.label,
    newText: item.textEdit.newText,
    documentation: item.documentation,
    filterText: item.filterText ?? null,
    range: item.textEdit.range,
  };
}

async function stylesheetOracle(require, root) {
  const { doComplete } = require(path.join(root, "@vscode/emmet-helper"));
  const { TextDocument } = require(path.join(root, "vscode-languageserver-textdocument"));
  const server = path.join(root, "svelte-language-server/dist/src");
  const { CSSPlugin } = require(path.join(server, "plugins/css/CSSPlugin"));
  const { DocumentManager, Document } = require(path.join(server, "lib/documents"));
  const { LSConfigManager } = require(path.join(server, "ls-config"));
  const { createLanguageServices } = require(path.join(server, "plugins/css/service"));
  const emmet = require(path.join(root, "emmet"));
  const names = Object.keys(emmet.resolveConfig({ type: "stylesheet", syntax: "css" }).snippets);

  const cases = [
    ...["css", "scss", "less", "sass"].flatMap((syntax) =>
      STYLESHEET_CORPUS.map((marked) => [syntax, marked]),
    ),
    ...names.map((name) => ["css", name]),
  ];
  const lines = cases.map(([syntax, marked]) => {
    const at = marked.indexOf("‸");
    const line = marked.replace("‸", "");
    const cursor = at === -1 ? line.length : at;
    const document = TextDocument.create(`file:///corpus.${syntax}`, syntax, 0, line);
    const list = doComplete(document, { line: 0, character: cursor }, syntax, {});
    return { syntax, line, cursor, items: list ? list.items.map(item) : null };
  });

  // The plugin as the server builds it, with the default settings.
  const config = new LSConfigManager();
  const documents = new DocumentManager((document) => new Document(document.uri, document.text));
  const plugin = new CSSPlugin(
    documents,
    config,
    [{ uri: "file:///corpus", name: "corpus" }],
    createLanguageServices({}),
  );
  const answers = [];
  for (const marked of STYLE_DOCUMENTS) {
    const at = marked.indexOf("‸");
    if (at === -1) {
      throw new Error(`no cursor in ${JSON.stringify(marked)}`);
    }
    const text = marked.slice(0, at) + marked.slice(at + 1);
    const document = new Document("file:///corpus/Corpus.svelte", text);
    const position = document.positionAt(at);
    const list = await plugin.getCompletions(document, position);
    const items = (list?.items ?? []).filter((item) => item.detail === "Emmet Abbreviation");
    answers.push({ text, position, isIncomplete: list?.isIncomplete ?? null, items: items.map(item) });
  }
  return { lines, documents: answers };
}

const root = packageRoot();
const require = createRequire(import.meta.url);
fs.writeFileSync(DATA, `${JSON.stringify(data(require, root), null, 1)}\n`);
fs.writeFileSync(ORACLE, `${JSON.stringify(oracle(require, root), null, 1)}\n`);
fs.writeFileSync(
  STYLESHEET_ORACLE,
  `${JSON.stringify(await stylesheetOracle(require, root), null, 1)}\n`,
);
console.log(
  `wrote ${[DATA, ORACLE, STYLESHEET_ORACLE].map((file) => path.relative(ROOT, file)).join(", ")}`,
);

import { describe, expect, test } from "bun:test";
import { BasesError, MissingThisContextError } from "../../src/expr/errors";
import type { EvalContext } from "../../src/expr/evaluator";
import { evaluateExpression, isTruthy, toDisplayString } from "../../src/expr/evaluator";
import {
  type BasesValue,
  DateValue,
  DurationValue,
  type FileAccessors,
  FileValue,
  LinkValue,
} from "../../src/expr/values";

/** A vault of one note, enough to exercise link resolution and file methods. */
function makeCtx(
  options: {
    note?: Record<string, BasesValue>;
    links?: string[];
    tags?: string[];
    path?: string;
    thisPath?: string;
    thisNote?: Record<string, BasesValue>;
  } = {},
): EvalContext {
  const path = options.path ?? "Notes/Alpha.md";
  const linkTargets = options.links ?? [];
  const accessors: FileAccessors = {
    tags: () => options.tags ?? [],
    links: () => linkTargets.map((t) => new LinkValue(t, undefined, t)),
    embeds: () => [],
    backlinks: () => [],
    properties: () => options.note ?? {},
    ctime: () => DateValue.fromParts(2020, 1, 1, 0, 0, 0, 0, true),
    mtime: () => DateValue.fromParts(2026, 1, 1, 0, 0, 0, 0, true),
    size: () => 42,
    tasks: () => [],
    resolve: (target) => new FileValue(target, accessors),
    linksTo: () => false,
  };
  const file = new FileValue(path, accessors);
  return {
    note: options.note ?? {},
    file,
    formula: {},
    thisValue:
      options.thisPath === undefined
        ? undefined
        : {
            file: new FileValue(options.thisPath, accessors),
            note: options.thisNote ?? {},
          },
  };
}

function ev(expr: string, ctx = makeCtx()): BasesValue {
  return evaluateExpression(expr, ctx);
}

describe("literals and arithmetic", () => {
  test("numbers, strings and booleans", () => {
    expect(ev("1 + 2")).toBe(3);
    expect(ev('"a" + "b"')).toBe("ab");
    expect(ev('1 + "x"')).toBe("1x");
    expect(ev("2.5.round()")).toBe(3);
    expect(ev("(2.5).round()")).toBe(3);
    expect(ev("(-5).abs()")).toBe(5);
    expect(ev("(3.14159).toFixed(2)")).toBe("3.14");
    expect(ev("7 % 3")).toBe(1);
  });

  test("division by zero yields 0 rather than throwing", () => {
    expect(ev("1 / 0")).toBe(0);
  });

  test("precedence follows JavaScript", () => {
    expect(ev("1 + 2 * 3")).toBe(7);
    expect(ev("(1 + 2) * 3")).toBe(9);
    expect(ev("!true && false")).toBe(false);
  });
});

describe("this binding", () => {
  test("this.file.name resolves to the host note basename", () => {
    const ctx = makeCtx({ thisPath: "Projects/SomeProject.md" });
    expect(ev("this.file.name", ctx)).toBe("SomeProject");
    expect(ev("this.file.path", ctx)).toBe("Projects/SomeProject.md");
  });

  test("this.path works as a direct File member", () => {
    const ctx = makeCtx({ thisPath: "Projects/SomeProject.md" });
    expect(ev("this.path", ctx)).toBe("Projects/SomeProject.md");
  });

  test("this also answers to host frontmatter", () => {
    const ctx = makeCtx({
      thisPath: "Projects/SomeProject.md",
      thisNote: { status: "active" },
    });
    expect(ev("this.status", ctx)).toBe("active");
    expect(ev("this.note.status", ctx)).toBe("active");
  });

  test("a base using `this` with no host is a hard error, not an empty result", () => {
    expect(() => ev("project.contains(link(this.file.name))")).toThrow(MissingThisContextError);
    expect(() => ev("this.file.name")).toThrow(MissingThisContextError);
  });
});

describe("the user's Tickets.base filter", () => {
  const filter = 'file.hasTag("ticket") && project.contains(link(this.file.name))';

  test("matches a note whose project links to the host", () => {
    const ctx = makeCtx({
      thisPath: "SomeProject.md",
      tags: ["ticket"],
      note: {
        project: [new LinkValue("SomeProject", undefined, "SomeProject.md")],
      },
    });
    expect(isTruthy(ev(filter, ctx))).toBe(true);
  });

  test("does not match a note belonging to a different project", () => {
    const ctx = makeCtx({
      thisPath: "OtherProject.md",
      tags: ["ticket"],
      note: {
        project: [new LinkValue("SomeProject", undefined, "SomeProject.md")],
      },
    });
    expect(isTruthy(ev(filter, ctx))).toBe(false);
  });

  test("does not match an untagged note even with the right project", () => {
    const ctx = makeCtx({
      thisPath: "SomeProject.md",
      tags: [],
      note: {
        project: [new LinkValue("SomeProject", undefined, "SomeProject.md")],
      },
    });
    expect(isTruthy(ev(filter, ctx))).toBe(false);
  });

  test("link equality ignores the display alias", () => {
    const ctx = makeCtx({
      thisPath: "SomeProject.md",
      tags: ["ticket"],
      note: {
        project: [new LinkValue("SomeProject.md", "Some Project", "SomeProject.md")],
      },
    });
    expect(isTruthy(ev(filter, ctx))).toBe(true);
  });
});

describe("file.* properties", () => {
  test("file.name is the basename without extension (probed on Obsidian 1.13.7)", () => {
    expect(ev("file.name", makeCtx({ path: "Notes/Alpha.md" }))).toBe("Alpha");
    expect(ev("file.basename", makeCtx({ path: "Notes/Alpha.md" }))).toBe("Alpha");
    expect(ev("file.path", makeCtx({ path: "Notes/Alpha.md" }))).toBe("Notes/Alpha.md");
    expect(ev("file.ext", makeCtx({ path: "Notes/Alpha.md" }))).toBe("md");
    expect(ev("file.folder", makeCtx({ path: "Notes/Alpha.md" }))).toBe("Notes");
  });

  test('a vault-root note reports folder "/", not the empty string', () => {
    // Probed on Obsidian 1.13.7: `Root Project.md` emits `"folder": "/"`,
    // while `Projects/SomeProject.md` emits `"folder": "Projects"`.
    expect(ev("file.folder", makeCtx({ path: "Alpha.md" }))).toBe("/");
    expect(ev('file.folder == ""', makeCtx({ path: "Alpha.md" }))).toBe(false);
    expect(ev('file.folder == "/"', makeCtx({ path: "Alpha.md" }))).toBe(true);
  });

  test("hasTag matches nested tags and strips a leading #", () => {
    const ctx = makeCtx({ tags: ["#plugin/transformer"] });
    expect(ev('file.hasTag("plugin")', ctx)).toBe(true);
    expect(ev('file.hasTag("plugin/transformer")', ctx)).toBe(true);
    expect(ev('file.hasTag("#plugin")', ctx)).toBe(true);
    expect(ev('file.hasTag("other")', ctx)).toBe(false);
  });

  test("inFolder is recursive, folder equality is not", () => {
    const ctx = makeCtx({ path: "a/b/c/Note.md" });
    expect(ev('file.inFolder("a")', ctx)).toBe(true);
    expect(ev('file.inFolder("a/b")', ctx)).toBe(true);
    expect(ev('file.folder == "a"', ctx)).toBe(false);
    expect(ev('file.folder == "a/b/c"', ctx)).toBe(true);
  });

  test("hasLink resolves an outbound link", () => {
    const ctx = makeCtx({ path: "Alpha.md", links: ["Beta.md"] });
    expect(ev('file.hasLink("Beta")', ctx)).toBe(true);
    expect(ev('file.hasLink("Gamma")', ctx)).toBe(false);
  });
});

describe("contains() dispatches on the receiver type", () => {
  test("string.contains is a substring test", () => {
    expect(ev('"hello".contains("ell")')).toBe(true);
    expect(ev('"hello".contains("xyz")')).toBe(false);
  });

  test("list.contains is a whole-element match", () => {
    expect(ev("[1,2,3].contains(2)")).toBe(true);
    expect(ev("[1,2,3].contains(9)")).toBe(false);
    expect(ev('"march".contains("3 medio march")')).toBe(false);
  });

  test("a type-strict mismatch throws rather than returning a wrong answer", () => {
    const ctx = makeCtx({ note: { n: 1 } });
    expect(() => ev("n.contains(1)", ctx)).toThrow(BasesError);
  });

  test("an empty list is falsy, which is what makes if() null-guards work", () => {
    expect(isTruthy(ev("[]"))).toBe(false);
    expect(ev("if([], 1, 2)")).toBe(2);
    expect(ev("if([1], 1, 2)")).toBe(1);
  });

  test("isEmpty is true for absent values", () => {
    const ctx = makeCtx({ note: { s: "hello" } });
    expect(ev("missing.isEmpty()", ctx)).toBe(true);
    expect(ev("s.isEmpty()", ctx)).toBe(false);
    expect(ev("[].isEmpty()", ctx)).toBe(true);
    expect(ev('"".isEmpty()', ctx)).toBe(true);
  });

  test("date.isEmpty() is always false, per the docs", () => {
    expect(ev('date("2024-01-01").isEmpty()')).toBe(false);
  });
});

describe("dates, durations and the two conflicting idioms", () => {
  test("date subtraction yields a Duration with .days", () => {
    const v = ev('(date("2026-06-11") - date("2026-06-10"))');
    expect(v).toBeInstanceOf(DurationValue);
    expect((v as DurationValue).days).toBe(1);
  });

  test("number() on a Duration yields ms, so the documented idiom works too", () => {
    const v = ev('((number(date("2026-06-11")) - number(date("2026-06-10"))) / 86400000).floor()');
    expect(v).toBe(1);
  });

  test("M is a month and m is a minute", () => {
    const a = ev('date("2024-12-01") + "1M"');
    expect((a as DateValue).toString()).toBe("2025-01-01");
    const b = ev('date("2024-12-01") + "1m"');
    expect((b as DateValue).toString()).toBe("2024-12-01T00:01:00");
  });

  test("duration must be on the left of a scalar product", () => {
    expect(() => ev("2 * duration('1d')")).toThrow(BasesError);
    expect(toDisplayString(ev("duration('1d') * 2"))).toBe("172800000 ms");
  });

  test("date() rejects garbage with Obsidian's error wording", () => {
    expect(() => ev('date("garbage")')).toThrow(/Invalid date format/);
  });

  test("date format tokens", () => {
    expect(ev('date("2026-05-26").format("YYYY-MM-DD")')).toBe("2026-05-26");
    expect(ev('date("2026-05-26").format("YYYY-MM")')).toBe("2026-05");
  });
});

describe("regex", () => {
  test("a g flag decides replace-first vs replace-all", () => {
    expect(ev('"a:b:c:d".replace(/:/, "-")')).toBe("a-b:c:d");
    expect(ev('"a:b:c:d".replace(/:/g, "-")')).toBe("a-b-c-d");
  });

  test("capture groups expand in the replacement", () => {
    expect(ev('"John Smith".replace(/(\\w+) (\\w+)/, "$2, $1")')).toBe("Smith, John");
  });

  test("regex.matches works and division still parses", () => {
    expect(ev('/^\\d{4}-\\d{2}-\\d{2}$/.matches("2026-05-26")')).toBe(true);
    expect(ev("10 / 2")).toBe(5);
  });
});

describe("higher-order list methods", () => {
  test("filter, map and reduce bind value/index/acc", () => {
    expect(ev("list([1,2,3]).filter(value > 1)")).toEqual([2, 3]);
    expect(ev("list([1,2,3]).map(value * 2)")).toEqual([2, 4, 6]);
    expect(ev("list([1,2,3]).reduce(acc + value, 0)")).toBe(6);
  });

  test("the documented max idiom works", () => {
    expect(
      ev(
        'values.filter(value.isType("number")).reduce(if(acc == null || value > acc, value, acc), null)',
        {
          ...makeCtx(),
          bindings: { values: [1, 9, 4] },
        },
      ),
    ).toBe(9);
  });

  test("map can produce a formatted date, as TaskNotes does", () => {
    expect(ev('list(["2026-05-26"]).map(date(value).format("YYYY-MM-DD"))')).toEqual([
      "2026-05-26",
    ]);
  });
});

describe("property access", () => {
  test("a bare identifier is a note property", () => {
    expect(ev("status", makeCtx({ note: { status: "active" } }))).toBe("active");
    expect(ev("note.status", makeCtx({ note: { status: "active" } }))).toBe("active");
  });

  test("a missing property is null, not a throw", () => {
    expect(ev("missing", makeCtx())).toBeNull();
    expect(ev("missing.deeper", makeCtx())).toBeNull();
  });

  test("bracket access works for names with spaces or hyphens", () => {
    const ctx = makeCtx({ note: { "sowing-time": ["march", "april"], "My Property": 7 } });
    expect(ev('note["sowing-time"]', ctx)).toEqual(["march", "april"]);
    expect(ev('note["My Property"]', ctx)).toBe(7);
  });

  test("formula namespace is readable", () => {
    const ctx = { ...makeCtx(), formula: { ppu: "3.50" } };
    expect(ev("formula.ppu", ctx)).toBe("3.50");
  });
});

describe("hard errors instead of silent nulls", () => {
  test("an unknown function throws", () => {
    expect(() => ev("nosuchfn(1)")).toThrow(/Unknown function/);
  });

  test("a method on the wrong type throws", () => {
    expect(() => ev("5.contains(1)")).toThrow(BasesError);
  });
});

describe("display stringification", () => {
  test("lists join with a comma, links render as wikilinks", () => {
    expect(toDisplayString(["a", "b"])).toBe("a, b");
    expect(toDisplayString(new LinkValue("Some Note", undefined, "Some Note.md"))).toBe(
      "[[Some Note]]",
    );
  });

  test("null stringifies to the empty string, matching the CLI's md output", () => {
    expect(toDisplayString(null)).toBe("");
  });
});

describe("comparing a list against a scalar does not recurse", () => {
  // Regression: `tags == 1` used to wrap the pair in lists and re-enter the same
  // branch, overflowing the stack and killing the server. Any filter comparing a
  // list-valued property to a scalar hit it, so the corpus did not have to be
  // malformed -- an ordinary `tags == "x"` in a user's base would do it.
  const ctxWith = (tags: BasesValue, prop: string): EvalContext =>
    makeCtx({ note: { [prop]: tags } });

  test("a one-element list equals its bare element", () => {
    expect(ev('lst == "a"', ctxWith(["a"], "lst"))).toBe(true);
    expect(ev('lst == "b"', ctxWith(["a"], "lst"))).toBe(false);
  });

  test("a longer list never equals a bare scalar", () => {
    expect(ev('lst == "a"', ctxWith(["a", "b"], "lst"))).toBe(false);
    expect(ev('lst != "a"', ctxWith(["a", "b"], "lst"))).toBe(true);
  });

  test("comparing a list to a number, null or boolean is false, not a crash", () => {
    expect(ev("lst == 1", ctxWith(["a"], "lst"))).toBe(false);
    expect(ev("lst == null", ctxWith(["a"], "lst"))).toBe(false);
    expect(ev("lst == true", ctxWith(["a"], "lst"))).toBe(false);
    expect(ev("lst == []", ctxWith([], "lst"))).toBe(true);
  });

  test("an empty list against a scalar is false in both directions", () => {
    expect(ev("lst == 0", ctxWith([], "lst"))).toBe(false);
    expect(ev("lst != 0", ctxWith([], "lst"))).toBe(true);
  });
});

describe("ordering coerces across types, and declines when it cannot", () => {
  // Measured against `obsidian base:query`. Obsidian's ComparisonExpr coerces a
  // string to a number on the right of a relational operator and returns null
  // for a null side, so `n < "10"` is numeric while `abc > 1` is false for
  // ToNumber("abc") being NaN.
  const num = makeCtx({ note: { n: 5 } });
  const abc = makeCtx({ note: { abc: "abc" } });

  test("a number compares numerically against a string", () => {
    expect(ev('n > "3"', num)).toBe(true);
    expect(ev('n >= "3"', num)).toBe(true);
    expect(ev('n < "10"', num)).toBe(true);
    expect(ev("n > 10", num)).toBe(false);
  });

  test("a string that does not convert makes every operator false", () => {
    expect(ev("abc > 1", abc)).toBe(false);
    expect(ev("abc < 1", abc)).toBe(false);
    expect(ev("abc >= 1", abc)).toBe(false);
    expect(ev("abc <= 1", abc)).toBe(false);
  });

  test("an ordering against null is false, not true", () => {
    // `compare` ranks null lowest because a sort needs it to; the filter must
    // not inherit that, or every note missing the property matches.
    expect(ev("missing < 10", num)).toBe(false);
    expect(ev("n < missing", num)).toBe(false);
    expect(ev("missing >= 0", num)).toBe(false);
    expect(ev("missing <= 0", num)).toBe(false);
    expect(ev("n < 10", num)).toBe(true);
  });
});

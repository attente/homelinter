import { describe, expect, it } from "vitest";
import { addCountedToast, addToast, MAX_TOASTS, removeToast } from "./toasts";

describe("addToast", () => {
  it("appends notices with distinct keys", () => {
    const list = addToast(addToast([], "a", "one"), "b", "two");
    expect(list.map((t) => t.msg)).toEqual(["one", "two"]);
  });

  it("merges a repeated notice into one entry and counts it", () => {
    let list = addToast([], "a", "one");
    list = addToast(list, "b", "two");
    list = addToast(list, "a", "one");
    expect(list).toEqual([
      { key: "a", msg: "one", count: 2 },
      { key: "b", msg: "two", count: 1 },
    ]);
  });

  it("caps the list and drops the oldest entry", () => {
    let list = addToast([], "first", "first");
    for (let i = 0; i < MAX_TOASTS; i += 1) {
      list = addToast(list, `n${i}`, `n${i}`);
    }
    expect(list).toHaveLength(MAX_TOASTS);
    expect(list.some((t) => t.key === "first")).toBe(false);
    expect(list.at(-1)?.msg).toBe(`n${MAX_TOASTS - 1}`);
  });

  it("keeps a merged entry where it already is", () => {
    let list = addToast([], "first", "first");
    for (let i = 0; i < MAX_TOASTS - 1; i += 1) {
      list = addToast(list, `n${i}`, `n${i}`);
    }
    list = addToast(list, "first", "first");
    expect(list.find((t) => t.key === "first")?.count).toBe(2);
    expect(list.map((t) => t.key).indexOf("first")).toBe(0);
  });
});

describe("addCountedToast", () => {
  const label = (n: number) => `rejected ${n} suggestion(s)`;

  it("renders the running total across merges", () => {
    let list = addCountedToast([], "revalidation", 1, label);
    list = addCountedToast(list, "revalidation", 2, label);
    expect(list).toEqual([
      { key: "revalidation", msg: "rejected 3 suggestion(s)", count: 3 },
    ]);
  });

  it("starts a fresh total after the toast was dismissed", () => {
    let list = addCountedToast([], "revalidation", 2, label);
    list = removeToast(list, "revalidation");
    list = addCountedToast(list, "revalidation", 1, label);
    expect(list[0].msg).toBe("rejected 1 suggestion(s)");
  });
});

describe("removeToast", () => {
  it("drops only the named entry", () => {
    const list = removeToast(addToast([], "a", "one"), "b");
    expect(list.map((t) => t.key)).toEqual(["a"]);
  });

  it("ignores an absent key", () => {
    expect(removeToast([], "missing")).toEqual([]);
  });
});

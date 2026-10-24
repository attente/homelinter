import { describe, expect, it } from "vitest";
import { dismissTopmost, registerModalClose } from "./modal";

describe("registerModalClose", () => {
  it("dismisses the most recently opened modal only", () => {
    const closed: string[] = [];
    const removeOuter = registerModalClose(() => closed.push("outer"));
    const removeInner = registerModalClose(() => closed.push("inner"));

    expect(dismissTopmost()).toBe(true);
    expect(dismissTopmost()).toBe(true);
    expect(dismissTopmost()).toBe(false);
    expect(closed).toEqual(["inner", "outer"]);
    removeInner();
    removeOuter();
  });

  it("keeps the stack honest when a lower modal closes first", () => {
    const closed: string[] = [];
    const removeOuter = registerModalClose(() => closed.push("outer"));
    const removeInner = registerModalClose(() => closed.push("inner"));

    removeInner();
    expect(dismissTopmost()).toBe(true);
    expect(closed).toEqual(["outer"]);
    expect(dismissTopmost()).toBe(false);
    removeOuter();
    // Unregistering an already dismissed entry is a no-op.
    expect(() => removeOuter()).not.toThrow();
    expect(dismissTopmost()).toBe(false);
  });

  it("allows re-registering after the stack empties", () => {
    const closed: string[] = [];
    const remove = registerModalClose(() => closed.push("first"));
    remove();
    expect(dismissTopmost()).toBe(false);

    const removeAgain = registerModalClose(() => closed.push("second"));
    expect(dismissTopmost()).toBe(true);
    expect(closed).toEqual(["second"]);
    removeAgain();
    expect(dismissTopmost()).toBe(false);
  });
});

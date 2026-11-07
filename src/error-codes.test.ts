import { describe, expect, it } from "vitest";
import { ErrorCodes } from "./error-codes";
import { errorOf } from "./types";

describe("error code compatibility", () => {
  it("preserves unknown upstream codes", () => {
    const error = {
      code: "VENDOR_EXTENDED_FAILURE",
      message: "fixture upstream failure",
    };

    expect(errorOf(error)).toEqual(error);
  });

  it("uses UNKNOWN for values without an error object", () => {
    expect(errorOf(null)).toEqual({
      code: ErrorCodes.Unknown,
      message: "操作失败，请重新尝试",
    });
  });
});

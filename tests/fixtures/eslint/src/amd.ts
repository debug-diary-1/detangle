declare function define(deps: string[], f: (...args: unknown[]) => void): void;

define(["require", "./legacy/old"], function () {});

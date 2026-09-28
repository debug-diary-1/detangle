declare const process: { getBuiltinModule(id: string): unknown };

export const fs = process.getBuiltinModule("fs");

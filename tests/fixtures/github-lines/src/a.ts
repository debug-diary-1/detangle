// a and b import each other.
import { b } from "./b";
import "./missing";

export const a = b;

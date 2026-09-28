import { api } from "../orders/api";
import "./ui/button";

// The same import string twice: both are reported.
export { api as ordersApi } from "../orders/api";
export const cart = api;

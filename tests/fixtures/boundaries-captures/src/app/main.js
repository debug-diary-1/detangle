import { auth } from "../modules/auth/index.js";
import { cart } from "../modules/cart/index.js";
import { format } from "../helpers/format/index.js";
import { stuff } from "../misc/stuff.js";
import { other } from "./other.js";
export const main = [auth, cart, format, stuff, other];

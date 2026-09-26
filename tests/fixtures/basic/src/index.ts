import React from "react";
import { cart } from "@/features/cart/cart";
import { user } from "./features/user/user.js";
import fs from "node:fs";
export const app = () => [React, cart, user, fs];

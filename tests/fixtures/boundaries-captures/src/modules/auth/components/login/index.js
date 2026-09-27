import { format } from "../../../../helpers/format/index.js";
import { other } from "../other/index.js";
import { list } from "../../../cart/components/list/index.js";
import _ from "lodash";
import { auth } from "../../index.js";
export const login = [format, other, list, _, auth];

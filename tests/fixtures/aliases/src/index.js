import Button from "@components/Button";   // webpack alias (prefix)
import utils from "utils";                   // webpack alias (exact, utils$)
import other from "utils/other";             // utils$ is exact: stays unresolved
import legacy from "legacy-lib";             // webpack alias false: ignored
import theme from "theme";                   // webpack resolve.modules → src/shared/theme.js
import Card from "~/components/Card";       // babel alias
import cart from "@feature/cart";            // babel regex alias → src/features/cart/index.js
import rootmod from "rootmod";               // babel root → src/roots/rootmod.js
import lib from "@lib/strings";              // detangle.toml alias
export default [Button, utils, other, legacy, theme, Card, cart, rootmod, lib];

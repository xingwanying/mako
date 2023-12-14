const assert = require("assert");
const { parseBuildResult, moduleReg } = require("../../../scripts/test-utils");
const { files } = parseBuildResult(__dirname);

let content = files["umi.css"];
content = content.replace(/\s/g, "");

assert(content.includes(`left:24px`), "should support math");

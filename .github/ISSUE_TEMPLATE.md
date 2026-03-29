<!--
The issue subtitle, used to provide another hook into the issue context despite its title
Example:
```mdx
### This doesn't work!!
```
-->
## Subtitle!

<!-- 
A quick acces on the locations pointed by this issue, includes file name amd line range for easy understanding at a glance.
Can provide multiple sources/locations.

Example:
```mdx
**Location**:
- [`main.rs`:13-77](link/to/diff)
- [`lib.rs`:13-77](link/to/diff)
- [`util.rs`:13-77](link/to/diff)
```
 -->
**Location**: [`main.rs`:13-77](#link/to/diff)

<!-- 
Quick snippet of the main location/source pointed as the issue for quick compression of the problem without the need to visit the diff links provided above.
Can provide more than one snippet, recommended attatching filename where teh snippet is located at the top as in the exaple below.

Example:
```rs
# src/main.rs
fn main() {
    println!("Hello Rust!")
}
```
 -->
```bash
# src/script.sh
echo "Hello World!"
```

<!-- 
If applicable (mostly always) provide more specific data/info.
Such as your operating system, architecture type, version of some that might be required to run the package and the package version.
Can provide more fields if applicable or related to this issue.

Example:
**Meta**:
- Windoware
- x64
- jailbreak.ps1 4.4.4
- Package 6.0.6
 -->
**Meta**:
- TempleOS 1.4.7
- x86
- Package 0.2.1

<!-- 
The assertive description about the issue, providing details, information, etc...

Example:
**Issue**: Some problem at line 5
 -->
**Issue**: It doesn't work.

<!-- 
If you know more in depth what is going on, have some context etc, you may provide a suggestion on how to fix it.

Example:
**Fix**: I think if you don't unplug your computer it might work.
 -->
**Fix**: Fix it, make no mistakes!

<!-- 
If you really really know in depth about it and have analyzed the source code reagrding this issue,
then you may provide a quick snippet exemplification of the fix suggestion provided by you.

Example:
```py
# src/pkg/__init__.py
from .core import CoreClass
from .utils import utilString, utilDict

__all__ = ["CoreClass", "utilString", "utilDict"]
```
 -->
```java
// app/io/some/xyz/domain/app/main.java
public class Janitor {
    public static void main(String[] args) {
        System.out.println("#NullGarbageCollectedPointerException!");
    }
}
```

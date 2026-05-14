// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! SPDX license identifier to header text mappings.
//!
//! Contains a self-contained registry of common SPDX license identifiers
//! and their standard short header text used at the top of source files.

use crate::error::HeatherError;

/// A known SPDX license definition.
struct LicenseDefinition {
    /// SPDX short identifier (e.g., `"MIT"`).
    id: &'static str,
    /// Standard header text for source files (without comment markers).
    header: &'static str,
}

/// Registry of all known SPDX license definitions.
const LICENSES: &[LicenseDefinition] = &[
    LicenseDefinition {
        id: "MIT",
        header: "Licensed under the MIT License.",
    },
    LicenseDefinition {
        id: "Apache-2.0",
        header: "\
Licensed under the Apache License, Version 2.0 (the \"License\");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at

    http://www.apache.org/licenses/LICENSE-2.0

Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an \"AS IS\" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.",
    },
    LicenseDefinition {
        id: "GPL-2.0-only",
        header: "\
This program is free software; you can redistribute it and/or modify
it under the terms of the GNU General Public License as published by
the Free Software Foundation; version 2.

This program is distributed in the hope that it will be useful,
but WITHOUT ANY WARRANTY; without even the implied warranty of
MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
GNU General Public License for more details.",
    },
    LicenseDefinition {
        id: "GPL-2.0-or-later",
        header: "\
This program is free software; you can redistribute it and/or modify
it under the terms of the GNU General Public License as published by
the Free Software Foundation; either version 2 of the License, or
(at your option) any later version.

This program is distributed in the hope that it will be useful,
but WITHOUT ANY WARRANTY; without even the implied warranty of
MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
GNU General Public License for more details.",
    },
    LicenseDefinition {
        id: "GPL-3.0-only",
        header: "\
This program is free software: you can redistribute it and/or modify
it under the terms of the GNU General Public License as published by
the Free Software Foundation, version 3.

This program is distributed in the hope that it will be useful,
but WITHOUT ANY WARRANTY; without even the implied warranty of
MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
GNU General Public License for more details.",
    },
    LicenseDefinition {
        id: "GPL-3.0-or-later",
        header: "\
This program is free software: you can redistribute it and/or modify
it under the terms of the GNU General Public License as published by
the Free Software Foundation, either version 3 of the License, or
(at your option) any later version.

This program is distributed in the hope that it will be useful,
but WITHOUT ANY WARRANTY; without even the implied warranty of
MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
GNU General Public License for more details.",
    },
    LicenseDefinition {
        id: "LGPL-2.1-only",
        header: "\
This library is free software; you can redistribute it and/or modify
it under the terms of the GNU Lesser General Public License as published by
the Free Software Foundation; version 2.1.

This library is distributed in the hope that it will be useful,
but WITHOUT ANY WARRANTY; without even the implied warranty of
MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
GNU Lesser General Public License for more details.",
    },
    LicenseDefinition {
        id: "LGPL-2.1-or-later",
        header: "\
This library is free software; you can redistribute it and/or modify
it under the terms of the GNU Lesser General Public License as published by
the Free Software Foundation; either version 2.1 of the License, or
(at your option) any later version.

This library is distributed in the hope that it will be useful,
but WITHOUT ANY WARRANTY; without even the implied warranty of
MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
GNU Lesser General Public License for more details.",
    },
    LicenseDefinition {
        id: "LGPL-3.0-only",
        header: "\
This library is free software: you can redistribute it and/or modify
it under the terms of the GNU Lesser General Public License as published by
the Free Software Foundation, version 3.

This library is distributed in the hope that it will be useful,
but WITHOUT ANY WARRANTY; without even the implied warranty of
MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
GNU Lesser General Public License for more details.",
    },
    LicenseDefinition {
        id: "LGPL-3.0-or-later",
        header: "\
This library is free software: you can redistribute it and/or modify
it under the terms of the GNU Lesser General Public License as published by
the Free Software Foundation, either version 3 of the License, or
(at your option) any later version.

This library is distributed in the hope that it will be useful,
but WITHOUT ANY WARRANTY; without even the implied warranty of
MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
GNU Lesser General Public License for more details.",
    },
    LicenseDefinition {
        id: "BSD-2-Clause",
        header: "\
Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the following conditions are met:

1. Redistributions of source code must retain the above copyright notice,
   this list of conditions and the following disclaimer.
2. Redistributions in binary form must reproduce the above copyright notice,
   this list of conditions and the following disclaimer in the documentation
   and/or other materials provided with the distribution.",
    },
    LicenseDefinition {
        id: "BSD-3-Clause",
        header: "\
Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the following conditions are met:

1. Redistributions of source code must retain the above copyright notice,
   this list of conditions and the following disclaimer.
2. Redistributions in binary form must reproduce the above copyright notice,
   this list of conditions and the following disclaimer in the documentation
   and/or other materials provided with the distribution.
3. Neither the name of the copyright holder nor the names of its contributors
   may be used to endorse or promote products derived from this software
   without specific prior written permission.",
    },
    LicenseDefinition {
        id: "ISC",
        header: "\
Permission to use, copy, modify, and/or distribute this software for any
purpose with or without fee is hereby granted, provided that the above
copyright notice and this permission notice appear in all copies.",
    },
    LicenseDefinition {
        id: "MPL-2.0",
        header: "\
This Source Code Form is subject to the terms of the Mozilla Public
License, v. 2.0. If a copy of the MPL was not distributed with this
file, You can obtain one at https://mozilla.org/MPL/2.0/.",
    },
    LicenseDefinition {
        id: "AGPL-3.0-only",
        header: "\
This program is free software: you can redistribute it and/or modify
it under the terms of the GNU Affero General Public License as published by
the Free Software Foundation, version 3.

This program is distributed in the hope that it will be useful,
but WITHOUT ANY WARRANTY; without even the implied warranty of
MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
GNU Affero General Public License for more details.",
    },
    LicenseDefinition {
        id: "AGPL-3.0-or-later",
        header: "\
This program is free software: you can redistribute it and/or modify
it under the terms of the GNU Affero General Public License as published by
the Free Software Foundation, either version 3 of the License, or
(at your option) any later version.

This program is distributed in the hope that it will be useful,
but WITHOUT ANY WARRANTY; without even the implied warranty of
MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
GNU Affero General Public License for more details.",
    },
    LicenseDefinition {
        id: "Unlicense",
        header: "\
This is free and unencumbered software released into the public domain.",
    },
    LicenseDefinition {
        id: "BSL-1.0",
        header: "\
Distributed under the Boost Software License, Version 1.0.",
    },
    LicenseDefinition {
        id: "0BSD",
        header: "\
Permission to use, copy, modify, and/or distribute this software for any
purpose with or without fee is hereby granted.",
    },
    LicenseDefinition {
        id: "Zlib",
        header: "\
This software is provided 'as-is', without any express or implied warranty.
In no event will the authors be held liable for any damages arising from
the use of this software.",
    },
];

/// Look up the standard header text for a given SPDX license identifier.
///
/// # Errors
///
/// Returns [`HeatherError::UnknownLicense`] if the identifier is not recognized.
pub fn header_for_license(spdx_id: &str) -> Result<&'static str, HeatherError> {
    LICENSES
        .iter()
        .find(|lic| lic.id.eq_ignore_ascii_case(spdx_id))
        .map(|lic| lic.header)
        .ok_or_else(|| HeatherError::UnknownLicense(spdx_id.to_owned()))
}

/// Returns a sorted list of all supported SPDX license identifiers.
#[must_use]
pub fn supported_licenses() -> Vec<&'static str> {
    let mut ids: Vec<&str> = LICENSES.iter().map(|lic| lic.id).collect();
    ids.sort_unstable();
    ids
}

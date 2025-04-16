#pragma once

#include <function_ref.hpp>

#include "utils/attributes.h"

namespace devilution {

/** Indicate if we only have access to demo data */
extern DVL_API_FOR_TEST bool gbIsSpawn;
/** Indicate if we have loaded the Hellfire expansion data */
extern DVL_API_FOR_TEST bool gbIsHellfire;
/** Indicate if we want vanilla savefiles */
extern DVL_API_FOR_TEST bool gbVanilla;
/** Whether the Hellfire mode is required (forced). */
extern bool forceHellfire;

/** Adds a handler to be called then `gbIsHellfire` changes after the initial startup. */
void AddIsHellfireChangeHandler(tl::function_ref<void()> callback);

} // namespace devilution

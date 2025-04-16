#include "ipc-helpers.hpp"
#include "ipc-method-repository.hpp"
#include "wayfire/plugin.hpp"
#include "wayfire/plugins/common/shared-core-data.hpp"
#include "wayfire/toplevel-view.hpp"
#include "wayfire/util.hpp"
#include <wayfire/view.hpp>
#include <wayfire/output.hpp>
#include <wayfire/workspace-set.hpp>
#include <wayfire/output-layout.hpp>
#include <wayfire/txn/transaction-manager.hpp>
#include "src/view/view-impl.hpp"
#include <variant>
#include <cstring>

#define WAYFIRE_PLUGIN
#include <wayfire/debug.hpp>
#include <wayfire/touch/touch.hpp>

extern "C" {
#include <wlr/backend/wayland.h>
#include <wlr/backend/multi.h>
#include <wlr/backend/headless.h>
#include <wlr/types/wlr_pointer.h>
#include <wlr/types/wlr_keyboard.h>
#include <wlr/interfaces/wlr_pointer.h>
#include <wlr/interfaces/wlr_keyboard.h>
#include <wlr/interfaces/wlr_touch.h>
#include <wlr/interfaces/wlr_tablet_tool.h>
#include <wlr/interfaces/wlr_tablet_pad.h>
#include <wlr/types/wlr_virtual_pointer_v1.h>
#include <wlr/types/wlr_virtual_keyboard_v1.h>
#include <wlr/types/wlr_output_layout.h>
#include <wlr/types/wlr_tablet_pad.h>
#include <wlr/types/wlr_tablet_tool.h>
#include <libevdev/libevdev.h>
}

#include <wayfire/util/log.hpp>
#include <wayfire/core.hpp>

static void locate_wayland_backend(wlr_backend *backend, void *data)
{
    if (wlr_backend_is_wl(backend))
    {
        wlr_backend **result = (wlr_backend**)data;
        *result = backend;
    }
}

namespace wf
{
static const struct wlr_pointer_impl pointer_impl = {
    .name = "stipc-pointer",
};

static void led_update(wlr_keyboard *keyboard, uint32_t leds)
{}

static const struct wlr_keyboard_impl keyboard_impl = {
    .name = "stipc-keyboard",
    .led_update = led_update,
};

static const struct wlr_touch_impl touch_impl = {
    .name = "stipc-touch-device",
};

static const struct wlr_tablet_impl tablet_impl = {
    .name = "stipc-tablet",
};

static const struct wlr_tablet_pad_impl tablet_pad_impl = {
    .name = "stipc-tablet-pad",
};

static void init_wlr_tool(wlr_tablet_tool *tablet_tool)
{
    std::memset(tablet_tool, 0, sizeof(*tablet_tool));
    tablet_tool->type     = WLR_TABLET_TOOL_TYPE_PEN;
    tablet_tool->pressure = true;
    wl_signal_init(&tablet_tool->events.destroy);
}

class headless_input_backend_t
{
  public:
    wlr_backend *backend;
    wlr_pointer pointer;
    wlr_keyboard keyboard;
    wlr_touch touch;
    wlr_tablet tablet;
    wlr_tablet_tool tablet_tool;
    wlr_tablet_pad tablet_pad;

    headless_input_backend_t()
    {
        auto& core = wf::get_core();
        backend = wlr_headless_backend_create(core.ev_loop);
        wlr_multi_backend_add(core.backend, backend);

        wlr_pointer_init(&pointer, &pointer_impl, "stipc_pointer");
        wlr_keyboard_init(&keyboard, &keyboard_impl, "stipc_keyboard");
        wlr_touch_init(&touch, &touch_impl, "stipc_touch");
        wlr_tablet_init(&tablet, &tablet_impl, "stipc_tablet_tool");
        wlr_tablet_pad_init(&tablet_pad, &tablet_pad_impl, "stipc_tablet_pad");
        init_wlr_tool(&tablet_tool);

        wl_signal_emit_mutable(&backend->events.new_input, &pointer.base);
        wl_signal_emit_mutable(&backend->events.new_input, &keyboard.base);
        wl_signal_emit_mutable(&backend->events.new_input, &touch.base);
        wl_signal_emit_mutable(&backend->events.new_input, &tablet.base);
        wl_signal_emit_mutable(&backend->events.new_input, &tablet_pad.base);

        if (core.get_current_state() == compositor_state_t::RUNNING)
        {
            wlr_backend_start(backend);
        }

        wl_signal_emit_mutable(&tablet_pad.events.attach_tablet, &tablet_tool);
    }

    ~headless_input_backend_t()
    {
        auto& core = wf::get_core();
        wlr_pointer_finish(&pointer);
        wlr_keyboard_finish(&keyboard);
        wlr_touch_finish(&touch);
        wlr_tablet_finish(&tablet);
        wlr_tablet_pad_finish(&tablet_pad);
        wlr_multi_backend_remove(core.backend, backend);
        wlr_backend_destroy(backend);
    }

    void do_key(uint32_t key, wl_keyboard_key_state state)
    {
        wlr_keyboard_key_event ev;
        ev.keycode = key;
        ev.state   = state;
        ev.update_state = true;
        ev.time_msec    = get_current_time();
        wlr_keyboard_notify_key(&keyboard, &ev);
    }

    void do_button(uint32_t button, wl_pointer_button_state state)
    {
        wlr_pointer_button_event ev;
        ev.pointer   = &pointer;
        ev.button    = button;
        ev.state     = state;
        ev.time_msec = get_current_time();
        wl_signal_emit(&pointer.events.button, &ev);
        wl_signal_emit(&pointer.events.frame, NULL);
    }

    void do_motion(double x, double y)
    {
        auto cursor = wf::get_core().get_cursor_position();

        wlr_pointer_motion_event ev;
        ev.pointer   = &pointer;
        ev.time_msec = get_current_time();
        ev.delta_x   = ev.unaccel_dx = x - cursor.x;
        ev.delta_y   = ev.unaccel_dy = y - cursor.y;
        wl_signal_emit(&pointer.events.motion, &ev);
        wl_signal_emit(&pointer.events.frame, NULL);
    }

    void convert_xy_to_relative(double *x, double *y)
    {
        auto layout = wf::get_core().output_layout->get_handle();
        wlr_box box;
        wlr_output_layout_get_box(layout, NULL, &box);
        *x = 1.0 * (*x - box.x) / box.width;
        *y = 1.0 * (*y - box.y) / box.height;
    }

    void do_touch(int finger, double x, double y)
    {
        convert_xy_to_relative(&x, &y);
        if (!wf::get_core().get_touch_state().fingers.count(finger))
        {
            wlr_touch_down_event ev;
            ev.touch     = &touch;
            ev.time_msec = get_current_time();
            ev.x = x;
            ev.y = y;
            ev.touch_id = finger;
            wl_signal_emit(&touch.events.down, &ev);
        } else
        {
            wlr_touch_motion_event ev;
            ev.touch     = &touch;
            ev.time_msec = get_current_time();
            ev.x = x;
            ev.y = y;
            ev.touch_id = finger;
            wl_signal_emit(&touch.events.motion, &ev);
        }

        wl_signal_emit(&touch.events.frame, NULL);
    }

    void do_touch_release(int finger)
    {
        wlr_touch_up_event ev;
        ev.touch     = &touch;
        ev.time_msec = get_current_time();
        ev.touch_id  = finger;
        wl_signal_emit(&touch.events.up, &ev);
        wl_signal_emit(&touch.events.frame, NULL);
    }

    void do_tablet_proximity(bool prox_in, double x, double y)
    {
        convert_xy_to_relative(&x, &y);
        wlr_tablet_tool_proximity_event ev;
        ev.tablet = &tablet;
        ev.tool   = &tablet_tool;
        ev.state  = prox_in ? WLR_TABLET_TOOL_PROXIMITY_IN : WLR_TABLET_TOOL_PROXIMITY_OUT;
        ev.time_msec = get_current_time();
        ev.x = x;
        ev.y = y;
        wl_signal_emit(&tablet.events.proximity, &ev);
    }

    void do_tablet_tip(bool tip_down, double x, double y)
    {
        convert_xy_to_relative(&x, &y);
        wlr_tablet_tool_tip_event ev;
        ev.tablet = &tablet;
        ev.tool   = &tablet_tool;
        ev.state  = tip_down ? WLR_TABLET_TOOL_TIP_DOWN : WLR_TABLET_TOOL_TIP_UP;
        ev.time_msec = get_current_time();
        ev.x = x;
        ev.y = y;
        wl_signal_emit(&tablet.events.tip, &ev);
    }

    void do_tablet_button(uint32_t button, bool down)
    {
        wlr_tablet_tool_button_event ev;
        ev.tablet = &tablet;
        ev.tool   = &tablet_tool;
        ev.button = button;
        ev.state  = down ? WLR_BUTTON_PRESSED : WLR_BUTTON_RELEASED;
        ev.time_msec = get_current_time();
        wl_signal_emit(&tablet.events.button, &ev);
    }

    void do_tablet_axis(double x, double y, double pressure)
    {
        convert_xy_to_relative(&x, &y);
        wlr_tablet_tool_axis_event ev;
        ev.tablet = &tablet;
        ev.tool   = &tablet_tool;
        ev.time_msec = get_current_time();
        ev.pressure  = pressure;
        ev.x = x;
        ev.y = y;

        ev.updated_axes = WLR_TABLET_TOOL_AXIS_X | WLR_TABLET_TOOL_AXIS_Y | WLR_TABLET_TOOL_AXIS_PRESSURE;
        wl_signal_emit(&tablet.events.axis, &ev);
    }

    void do_tablet_pad_button(uint32_t button, bool state)
    {
        wlr_tablet_pad_button_event ev;
        ev.group  = 0;
        ev.button = button;
        ev.state  = state ? WLR_BUTTON_PRESSED : WLR_BUTTON_RELEASED;
        ev.mode   = 0;
        ev.time_msec = get_current_time();
        wl_signal_emit(&tablet_pad.events.button, &ev);
    }

    headless_input_backend_t(const headless_input_backend_t&) = delete;
    headless_input_backend_t(headless_input_backend_t&&) = delete;
    headless_input_backend_t& operator =(const headless_input_backend_t&) = delete;
    headless_input_backend_t& operator =(headless_input_backend_t&&) = delete;
};

class stipc_plugin_t : public wf::plugin_interface_t
{
    wf::shared_data::ref_ptr_t<wf::ipc::method_repository_t> method_repository;

  public:
    void init() override
    {
        input = std::make_unique<headless_input_backend_t>();
        method_repository->register_method("stipc/create_wayland_output", create_wayland_output);
        method_repository->register_method("stipc/destroy_wayland_output", destroy_wayland_output);
        method_repository->register_method("stipc/feed_key", feed_key);
        method_repository->register_method("stipc/feed_button", feed_button);
        method_repository->register_method("stipc/move_cursor", move_cursor);
        method_repository->register_method("stipc/run", run);
        method_repository->register_method("stipc/ping", ping);
        method_repository->register_method("stipc/get_display", get_display);
        method_repository->register_method("stipc/layout_views", layout_views);
        method_repository->register_method("stipc/touch", do_touch);
        method_repository->register_method("stipc/touch_release", do_touch_release);
        method_repository->register_method("stipc/tablet/tool_proximity", do_tool_proximity);
        method_repository->register_method("stipc/tablet/tool_button", do_tool_button);
        method_repository->register_method("stipc/tablet/tool_axis", do_tool_axis);
        method_repository->register_method("stipc/tablet/tool_tip", do_tool_tip);
        method_repository->register_method("stipc/tablet/pad_button", do_pad_button);
        method_repository->register_method("stipc/delay_next_tx", delay_next_tx);
        method_repository->register_method("stipc/get_xwayland_pid", get_xwayland_pid);
        method_repository->register_method("stipc/get_xwayland_display", get_xwayland_display);
    }

    bool is_unloadable() override
    {
        return false;
    }

    ipc::method_callback layout_views = [] (wf::json_t data) -> wf::json_t
    {
        auto views = wf::get_core().get_all_views();
        if (!data.has_member("views") || !data["views"].is_array())
        {
            return wf::ipc::json_error("Views not specified");
        }

        for (size_t i = 0; i < data["views"].size(); i++)
        {
            const auto& v = data["views"][i];
            auto id   = wf::ipc::json_get_uint64(v, "id");
            int x     = wf::ipc::json_get_int64(v, "x");
            int y     = wf::ipc::json_get_int64(v, "y");
            int width = wf::ipc::json_get_int64(v, "width");
            int height  = wf::ipc::json_get_int64(v, "height");
            auto output = wf::ipc::json_get_optional_string(v, "output");

            auto it = std::find_if(views.begin(), views.end(), [&] (auto& view)
            {
                return view->get_id() == uint64_t(v["id"]);
            });

            if (it == views.end())
            {
                return wf::ipc::json_error("Could not find view with id " +
                    std::to_string(id));
            }

            auto toplevel = toplevel_cast(*it);
            if (!toplevel)
            {
                return wf::ipc::json_error("View is not toplevel view id " +
                    std::to_string(id));
            }

            if (output.has_value())
            {
                auto wo = wf::get_core().output_layout->find_output(output.value());
                if (!wo)
                {
                    return wf::ipc::json_error("Unknown output " + (std::string)output.value());
                }

                move_view_to_output(toplevel, wo, false);
            }

            wf::geometry_t g{x, y, width, height};
            toplevel->set_geometry(g);
        }

        return wf::ipc::json_ok();
    };

    ipc::method_callback create_wayland_output = [] (wf::json_t)
    {
        auto backend = wf::get_core().backend;

        wlr_backend *wayland_backend = NULL;
        wlr_multi_for_each_backend(backend, locate_wayland_backend,
            &wayland_backend);

        if (!wayland_backend)
        {
            return wf::ipc::json_error("Wayfire is not running in nested wayland mode!");
        }

        wlr_wl_output_create(wayland_backend);
        return wf::ipc::json_ok();
    };

    ipc::method_callback destroy_wayland_output = [] (wf::json_t data) -> json_t
    {
        auto output_str = wf::ipc::json_get_string(data, "output");
        auto output     = wf::get_core().output_layout->find_output(output_str);
        if (!output)
        {
            return wf::ipc::json_error("Could not find output: \"" + output_str + "\"");
        }

        if (!wlr_output_is_wl(output->handle))
        {
            return wf::ipc::json_error("Output is not a wayland output!");
        }

        wlr_output_destroy(output->handle);
        return wf::ipc::json_ok();
    };

    struct key_t
    {
        bool modifier;
        int code;
    };

    std::variant<key_t, std::string> parse_key(wf::json_t data)
    {
        auto combo = wf::ipc::json_get_string(data, "combo");
        if (combo.size() < 4)
        {
            return std::string("Missing or wrong json type for `combo`!");
        }

        // Check super modifier
        bool modifier = false;
        if (combo.substr(0, 2) == "S-")
        {
            modifier = true;
            combo    = combo.substr(2);
        }

        int key = libevdev_event_code_from_name(EV_KEY, combo.c_str());
        if (key == -1)
        {
            return std::string("Failed to parse combo \"" + combo + "\"");
        }

        return key_t{modifier, key};
    }

    ipc::method_callback feed_key = [=] (wf::json_t data)
    {
        auto key    = wf::ipc::json_get_string(data, "key");
        auto state  = wf::ipc::json_get_bool(data, "state");
        int keycode = libevdev_event_code_from_name(EV_KEY, key.c_str());
        if (keycode == -1)
        {
            return wf::ipc::json_error("Failed to parse evdev key \"" + key + "\"");
        }

        if (state)
        {
            input->do_key(keycode, WL_KEYBOARD_KEY_STATE_PRESSED);
        } else
        {
            input->do_key(keycode, WL_KEYBOARD_KEY_STATE_RELEASED);
        }

        return wf::ipc::json_ok();
    };

    ipc::method_callback feed_button = [=] (wf::json_t data)
    {
        auto result = parse_key(data);
        auto button = std::get_if<key_t>(&result);
        if (!button)
        {
            return wf::ipc::json_error(std::get<std::string>(result));
        }

        auto mode = wf::ipc::json_get_string(data, "mode");
        if ((mode == "press") || (mode == "full"))
        {
            if (button->modifier)
            {
                input->do_key(KEY_LEFTMETA, WL_KEYBOARD_KEY_STATE_PRESSED);
            }

            input->do_button(button->code, WL_POINTER_BUTTON_STATE_PRESSED);
        }

        if ((mode == "release") || (mode == "full"))
        {
            input->do_button(button->code, WL_POINTER_BUTTON_STATE_RELEASED);
            if (button->modifier)
            {
                input->do_key(KEY_LEFTMETA, WL_KEYBOARD_KEY_STATE_RELEASED);
            }
        }

        return wf::ipc::json_ok();
    };

    ipc::method_callback move_cursor = [=] (wf::json_t data)
    {
        auto x = wf::ipc::json_get_double(data, "x");
        auto y = wf::ipc::json_get_double(data, "y");
        input->do_motion(x, y);
        return wf::ipc::json_ok();
    };

    ipc::method_callback do_touch = [=] (wf::json_t data)
    {
        auto finger = wf::ipc::json_get_int64(data, "finger");
        auto x = wf::ipc::json_get_double(data, "x");
        auto y = wf::ipc::json_get_double(data, "y");
        input->do_touch(finger, x, y);
        return wf::ipc::json_ok();
    };

    ipc::method_callback do_touch_release = [=] (wf::json_t data)
    {
        auto finger = wf::ipc::json_get_int64(data, "finger");
        input->do_touch_release(finger);
        return wf::ipc::json_ok();
    };

    ipc::method_callback run = [=] (wf::json_t data)
    {
        auto cmd = wf::ipc::json_get_string(data, "cmd");
        auto response = wf::ipc::json_ok();
        pid_t pid     = wf::get_core().run(cmd);
        if (!pid)
        {
            return wf::ipc::json_error("failed to run command");
        }

        response["pid"] = pid;
        return response;
    };

    ipc::method_callback ping = [=] (wf::json_t data)
    {
        return wf::ipc::json_ok();
    };

    ipc::method_callback get_display = [=] (wf::json_t data)
    {
        wf::json_t dpy;
        dpy["wayland"]  = wf::get_core().wayland_display;
        dpy["xwayland"] = wf::get_core().get_xwayland_display();
        return dpy;
    };

    ipc::method_callback do_tool_proximity = [=] (wf::json_t data)
    {
        auto proximity_in = wf::ipc::json_get_bool(data, "proximity_in");
        auto x = wf::ipc::json_get_double(data, "x");
        auto y = wf::ipc::json_get_double(data, "y");
        input->do_tablet_proximity(proximity_in, x, y);
        return wf::ipc::json_ok();
    };

    ipc::method_callback do_tool_button = [=] (wf::json_t data)
    {
        auto button = wf::ipc::json_get_int64(data, "button");
        auto state  = wf::ipc::json_get_bool(data, "state");
        input->do_tablet_button(button, state);
        return wf::ipc::json_ok();
    };

    ipc::method_callback do_tool_axis = [=] (wf::json_t data)
    {
        auto x = wf::ipc::json_get_double(data, "x");
        auto y = wf::ipc::json_get_double(data, "y");
        auto pressure = wf::ipc::json_get_double(data, "pressure");
        input->do_tablet_axis(x, y, pressure);
        return wf::ipc::json_ok();
    };

    ipc::method_callback do_tool_tip = [=] (wf::json_t data)
    {
        auto x     = wf::ipc::json_get_double(data, "x");
        auto y     = wf::ipc::json_get_double(data, "y");
        auto state = wf::ipc::json_get_bool(data, "state");
        input->do_tablet_tip(state, x, y);
        return wf::ipc::json_ok();
    };

    ipc::method_callback do_pad_button = [=] (wf::json_t data)
    {
        auto button = wf::ipc::json_get_int64(data, "button");
        auto state  = wf::ipc::json_get_bool(data, "state");
        input->do_tablet_pad_button(button, state);
        return wf::ipc::json_ok();
    };

    class never_ready_object : public wf::txn::transaction_object_t
    {
      public:
        void commit() override
        {}

        void apply() override
        {}

        std::string stringify() const override
        {
            return "force-timeout";
        }
    };

    int delay_counter = 0;
    wf::signal::connection_t<wf::txn::new_transaction_signal> on_new_tx =
        [=] (wf::txn::new_transaction_signal *ev)
    {
        ev->tx->add_object(std::make_shared<never_ready_object>());

        delay_counter--;
        if (delay_counter <= 0)
        {
            on_new_tx.disconnect();
        }
    };

    ipc::method_callback delay_next_tx = [=] (wf::json_t)
    {
        if (!on_new_tx.is_connected())
        {
            wf::get_core().tx_manager->connect(&on_new_tx);
        }

        ++delay_counter;
        return wf::ipc::json_ok();
    };

    ipc::method_callback get_xwayland_pid = [=] (wf::json_t)
    {
        auto response = wf::ipc::json_ok();
        response["pid"] = wf::xwayland_get_pid();
        return response;
    };

    ipc::method_callback get_xwayland_display = [=] (wf::json_t)
    {
        auto response = wf::ipc::json_ok();
        response["display"] = wf::xwayland_get_display();
        return response;
    };

    std::unique_ptr<headless_input_backend_t> input;
};
}

DECLARE_WAYFIRE_PLUGIN(wf::stipc_plugin_t);
